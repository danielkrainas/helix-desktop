//! `xdg_toplevel_icon_manager_v1` (helix-desktop-spec
//! `contracts/wayland/README.md`, "Window icons") between Helix's server
//! side and a real Wayland client, in-process.

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use helix_comp::toplevel_icon::{IconBuilder, IconCachedState, ToplevelIconHandler, ToplevelIcons};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel;
use smithay::reexports::wayland_protocols::xdg::toplevel_icon::v1::server as icon_server;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle};
use smithay::utils::Serial;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{with_states, CompositorClientState, CompositorHandler, CompositorState};
use smithay::wayland::shell::xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::input::{SeatHandler, SeatState};
use smithay::{delegate_compositor, delegate_seat, delegate_shm, delegate_xdg_shell};

// ---- the server ----

struct Server {
    compositor: CompositorState,
    xdg: XdgShellState,
    shm: ShmState,
    seat_state: SeatState<Server>,
    toplevels: Vec<ToplevelSurface>,
}

impl SeatHandler for Server {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
}

#[derive(Default)]
struct ClientInfo {
    compositor: CompositorClientState,
    gone: Arc<std::sync::atomic::AtomicBool>,
}

impl ClientData for ClientInfo {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {
        self.gone.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl CompositorHandler for Server {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientInfo>().unwrap().compositor
    }
    fn commit(&mut self, _: &WlSurface) {}
}

impl XdgShellHandler for Server {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg
    }
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.toplevels.push(surface);
    }
    fn new_popup(&mut self, _: PopupSurface, _: PositionerState) {}
    fn grab(&mut self, _: PopupSurface, _: WlSeat, _: Serial) {}
    fn reposition_request(&mut self, _: PopupSurface, _: PositionerState, _: u32) {}
}

impl BufferHandler for Server {
    fn buffer_destroyed(&mut self, _: &WlBuffer) {}
}

impl ShmHandler for Server {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}

impl ToplevelIconHandler for Server {
    fn toplevel_surface(&self, toplevel: &XdgToplevel) -> Option<WlSurface> {
        self.xdg.get_toplevel(toplevel).map(|t| t.wl_surface().clone())
    }
}

delegate_compositor!(Server);
delegate_seat!(Server);
delegate_xdg_shell!(Server);
delegate_shm!(Server);
smithay::reexports::wayland_server::delegate_global_dispatch!(Server: [icon_server::xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1: Vec<i32>] => ToplevelIcons);
smithay::reexports::wayland_server::delegate_dispatch!(Server: [icon_server::xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1: ()] => ToplevelIcons);
smithay::reexports::wayland_server::delegate_dispatch!(Server: [icon_server::xdg_toplevel_icon_v1::XdgToplevelIconV1: IconBuilder] => ToplevelIcons);

type Command = Box<dyn FnOnce(&mut Server) + Send>;

/// The server on its own thread, with one client on `stream`; also whether
/// that client was disconnected.
fn start_server(stream: UnixStream) -> (mpsc::Sender<Command>, Arc<std::sync::atomic::AtomicBool>) {
    let (tx, rx) = mpsc::channel::<Command>();
    let gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let info = ClientInfo { gone: gone.clone(), ..ClientInfo::default() };
    std::thread::spawn(move || {
        let mut display: Display<Server> = Display::new().unwrap();
        let dh: DisplayHandle = display.handle();
        let mut state = Server {
            compositor: CompositorState::new::<Server>(&dh),
            xdg: XdgShellState::new::<Server>(&dh),
            shm: ShmState::new::<Server>(&dh, Vec::new()),
            seat_state: SeatState::new(),
            toplevels: Vec::new(),
        };
        helix_comp::toplevel_icon::create_global::<Server>(&dh, &[24, 48]);
        display.handle().insert_client(stream, Arc::new(info)).unwrap();
        loop {
            loop {
                match rx.try_recv() {
                    Ok(command) => command(&mut state),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
            let _ = display.dispatch_clients(&mut state);
            let _ = display.flush_clients();
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    (tx, gone)
}

/// An icon as the server holds it: name, and image sizes with their
/// first pixel.
type Seen = (Option<String>, Vec<(u32, [u8; 4])>);

/// The icon the server's first toplevel has now.
fn server_icon(tx: &mpsc::Sender<Command>) -> Option<Seen> {
    let (reply, answer) = mpsc::channel();
    tx.send(Box::new(move |state: &mut Server| {
        let icon = state.toplevels.first().and_then(|t| with_states(t.wl_surface(), |s| s.cached_state.get::<IconCachedState>().current().icon.clone()));
        let _ = reply.send(icon.map(|i| (i.name.clone(), i.images.iter().map(|m| (m.size, [m.pixels[0], m.pixels[1], m.pixels[2], m.pixels[3]])).collect())));
    }))
    .unwrap();
    answer.recv_timeout(Duration::from_secs(5)).unwrap()
}

// ---- the client ----

mod client {
    use wayland_client::globals::GlobalListContents;
    use wayland_client::protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface};
    use wayland_client::{Connection, Dispatch, QueueHandle};
    use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
    use wayland_protocols::xdg::toplevel_icon::v1::client::{xdg_toplevel_icon_manager_v1, xdg_toplevel_icon_v1};

    #[derive(Default)]
    pub struct State {
        pub sizes: Vec<i32>,
        pub done: bool,
    }

    macro_rules! quiet {
        ($($iface:ty),*) => {$(
            impl Dispatch<$iface, ()> for State {
                fn event(_: &mut Self, _: &$iface, _: <$iface as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
            }
        )*};
    }
    quiet!(wl_compositor::WlCompositor, wl_surface::WlSurface, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, xdg_surface::XdgSurface, xdg_toplevel::XdgToplevel, xdg_toplevel_icon_v1::XdgToplevelIconV1);

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
    impl Dispatch<xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1, ()> for State {
        fn event(state: &mut Self, _: &xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1, event: xdg_toplevel_icon_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            match event {
                xdg_toplevel_icon_manager_v1::Event::IconSize { size } => state.sizes.push(size),
                xdg_toplevel_icon_manager_v1::Event::Done => state.done = true,
                _ => {}
            }
        }
    }
}

use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_buffer, wl_compositor, wl_shm, wl_surface};
use wayland_client::{Connection, EventQueue, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_toplevel, xdg_wm_base};
use wayland_protocols::xdg::toplevel_icon::v1::client::{xdg_toplevel_icon_manager_v1, xdg_toplevel_icon_v1};

struct App {
    conn: Connection,
    queue: EventQueue<client::State>,
    state: client::State,
    qh: QueueHandle<client::State>,
    shm: wl_shm::WlShm,
    toplevel: xdg_toplevel::XdgToplevel,
    surface: wl_surface::WlSurface,
    icons: xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1,
}

impl App {
    fn new(stream: UnixStream) -> Self {
        let conn = Connection::from_socket(stream).unwrap();
        let (globals, mut queue) = registry_queue_init::<client::State>(&conn).unwrap();
        let qh = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=6, ()).unwrap();
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
        let base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ()).unwrap();
        let icons: xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
        let surface = compositor.create_surface(&qh, ());
        let xdg = base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg.get_toplevel(&qh, ());
        surface.commit();
        let mut state = client::State::default();
        queue.roundtrip(&mut state).unwrap();
        Self { conn, queue, state, qh, shm, toplevel, surface, icons }
    }

    /// A `w`×`h` ARGB8888 buffer, every pixel `argb` (native order).
    fn buffer(&self, w: i32, h: i32, argb: u32) -> wl_buffer::WlBuffer {
        let bytes: Vec<u8> = std::iter::repeat_n(argb.to_ne_bytes(), (w * h) as usize).flatten().collect();
        // SAFETY: memfd_create returns a new fd we own (or -1).
        let raw = unsafe { libc::memfd_create(c"icon-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(raw >= 0);
        // SAFETY: just created.
        let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
        std::io::Write::write_all(&mut std::fs::File::from(fd.try_clone().unwrap()), &bytes).unwrap();
        let pool = self.shm.create_pool(fd.as_fd(), bytes.len() as i32, &self.qh, ());
        let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, &self.qh, ());
        pool.destroy();
        buffer
    }

    /// Send what is queued and handle the answers; false if the server
    /// disconnected us.
    fn sync(&mut self) -> bool {
        self.queue.roundtrip(&mut self.state).is_ok()
    }

    /// Send what is queued and read until the server ends the connection:
    /// the protocol error's code. (Sending more after the bad request
    /// would fail on the closed socket before the error is read.)
    fn error_code(&mut self) -> Option<u32> {
        self.conn.flush().ok()?;
        while self.queue.blocking_dispatch(&mut self.state).is_ok() {}
        self.conn.protocol_error().map(|e| e.code)
    }
}

fn pair() -> (App, mpsc::Sender<Command>, Arc<std::sync::atomic::AtomicBool>) {
    let (a, b) = UnixStream::pair().unwrap();
    let (tx, gone) = start_server(a);
    (App::new(b), tx, gone)
}

#[test]
fn chromium_sequence_keeps_the_client() {
    let (mut app, tx, gone) = pair();
    assert_eq!(app.state.sizes, [24, 48], "the sizes Helix wants");
    assert!(app.state.done);
    // As Chromium does it, all in one batch: give the icon to the window,
    // destroy the icon, then destroy its buffer, then commit.
    let icon = app.icons.create_icon(&app.qh, ());
    let buffer = app.buffer(4, 4, 0xff10_2030);
    icon.add_buffer(&buffer, 1);
    app.icons.set_icon(&app.toplevel, Some(&icon));
    icon.destroy();
    buffer.destroy();
    app.surface.commit();
    assert!(app.sync(), "still connected");
    std::thread::sleep(Duration::from_millis(50));
    assert!(!gone.load(std::sync::atomic::Ordering::SeqCst), "not disconnected");
    let (name, images) = server_icon(&tx).expect("the window has the icon");
    assert_eq!(name, None);
    assert_eq!(images, [(4, 0xff10_2030u32.to_ne_bytes())]);
    // Unsetting it.
    app.icons.set_icon(&app.toplevel, None);
    app.surface.commit();
    assert!(app.sync());
    assert!(server_icon(&tx).is_none());
    let _ = app.conn.flush();
}

#[test]
fn names_and_replacing_images() {
    let (mut app, tx, _) = pair();
    let icon = app.icons.create_icon(&app.qh, ());
    icon.set_name("org.example.App".into());
    let a = app.buffer(2, 2, 0xff00_0001);
    let b = app.buffer(2, 2, 0xff00_0002);
    let c = app.buffer(8, 8, 0xff00_0003);
    icon.add_buffer(&a, 1);
    icon.add_buffer(&b, 1);
    icon.add_buffer(&c, 1);
    app.icons.set_icon(&app.toplevel, Some(&icon));
    // Not shown until the surface commits.
    assert!(app.sync());
    assert!(server_icon(&tx).is_none());
    app.surface.commit();
    assert!(app.sync());
    let (name, mut images) = server_icon(&tx).unwrap();
    images.sort();
    assert_eq!(name.as_deref(), Some("org.example.App"));
    assert_eq!(images, [(2, 0xff00_0002u32.to_ne_bytes()), (8, 0xff00_0003u32.to_ne_bytes())], "the later 2×2 image replaced the earlier");
}

#[test]
fn a_non_square_buffer_is_refused() {
    let (mut app, _server, gone) = pair();
    let icon = app.icons.create_icon(&app.qh, ());
    let buffer = app.buffer(4, 2, 0xffff_ffff);
    icon.add_buffer(&buffer, 1);
    assert_eq!(app.error_code(), Some(xdg_toplevel_icon_v1::Error::InvalidBuffer as u32));
    std::thread::sleep(Duration::from_millis(50));
    assert!(gone.load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
fn an_icon_given_to_a_window_cannot_change() {
    let (mut app, _server, _) = pair();
    let icon = app.icons.create_icon(&app.qh, ());
    icon.set_name("a".into());
    app.icons.set_icon(&app.toplevel, Some(&icon));
    icon.set_name("b".into());
    assert_eq!(app.error_code(), Some(xdg_toplevel_icon_v1::Error::Immutable as u32));
}
