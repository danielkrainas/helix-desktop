//! A client that stops reading its Wayland socket while pointer motion
//! floods it: an ordinary connection is dropped once the socket and the
//! server library's buffer are full; the shell's (a large send buffer and a
//! large library buffer, as helix-comp sets up for `--shell`) holds seconds
//! of a 1000 Hz mouse and is still there when the client reads again.
//! In-process: a Smithay server on a thread and a wayland-client client.

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use helix_comp::client_socket::{self, DisconnectReason};
use smithay::input::pointer::{MotionEvent, PointerHandle};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle};
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState};
use smithay::{delegate_compositor, delegate_seat};

// ---- the server ----

struct Server {
    compositor: CompositorState,
    seat_state: SeatState<Server>,
    seat: Seat<Server>,
    surfaces: Vec<WlSurface>,
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
    gone: Arc<Mutex<Option<String>>>,
    /// The server's end of the socket (a second descriptor), to see what
    /// the client left unread, as helix-comp does for the shell.
    /// Closed when the client goes, or the connection would not end.
    socket: Mutex<Option<std::os::fd::OwnedFd>>,
}

impl ClientData for ClientInfo {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, reason: DisconnectReason) {
        let socket = self.socket.lock().unwrap().take();
        let unread = socket.as_ref().and_then(|s| client_socket::unread_bytes(s.as_fd()));
        *self.gone.lock().unwrap() = Some(client_socket::describe_disconnect(&reason, unread));
    }
}

impl CompositorHandler for Server {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientInfo>().unwrap().compositor
    }
    fn new_surface(&mut self, surface: &WlSurface) {
        self.surfaces.push(surface.clone());
    }
    fn commit(&mut self, _: &WlSurface) {}
}

delegate_compositor!(Server);
delegate_seat!(Server);

type Command = Box<dyn FnOnce(&mut Server, &mut Display<Server>) + Send>;

/// The server on its own thread with one client on `stream` (set up as the
/// shell's when `shell`); also why that client went, once it has.
fn start_server(stream: UnixStream, shell: bool) -> (mpsc::Sender<Command>, Arc<Mutex<Option<String>>>) {
    let (tx, rx) = mpsc::channel::<Command>();
    let gone = Arc::new(Mutex::new(None));
    let socket = Mutex::new(stream.try_clone().ok().map(std::os::fd::OwnedFd::from));
    let info = ClientInfo { gone: gone.clone(), socket, ..ClientInfo::default() };
    if shell {
        let granted = client_socket::enlarge_send_buffer(stream.as_fd(), client_socket::SHELL_SEND_BUFFER).unwrap();
        eprintln!("shell socket send buffer: {granted} bytes");
    }
    std::thread::spawn(move || {
        let mut display: Display<Server> = Display::new().unwrap();
        let dh: DisplayHandle = display.handle();
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "seat0");
        seat.add_pointer();
        let mut state = Server { compositor: CompositorState::new::<Server>(&dh), seat_state, seat, surfaces: Vec::new() };
        let client = display.handle().insert_client(stream, Arc::new(info)).unwrap();
        if shell {
            client.set_max_buffer_size(&dh, client_socket::SHELL_MAX_BUFFER);
        }
        loop {
            loop {
                match rx.try_recv() {
                    Ok(command) => command(&mut state, &mut display),
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

/// `seconds` of a 1000 Hz mouse moving over the client's surface: a motion
/// and a frame each millisecond (sent as fast as the server can, flushed as
/// a compositor would after each dispatch).
fn flood(tx: &mpsc::Sender<Command>, seconds: u32) {
    let (done, finished) = mpsc::channel();
    tx.send(Box::new(move |state: &mut Server, display: &mut Display<Server>| {
        let pointer: PointerHandle<Server> = state.seat.get_pointer().unwrap();
        let surface = state.surfaces.first().cloned().expect("the client's surface");
        for ms in 0..seconds * 1000 {
            let location = (f64::from(ms % 500), f64::from(ms % 300)).into();
            let focus = Some((surface.clone(), (0.0, 0.0).into()));
            pointer.motion(state, focus, &MotionEvent { location, serial: SERIAL_COUNTER.next_serial(), time: ms });
            pointer.frame(state);
            if ms % 8 == 0 {
                let _ = display.flush_clients();
            }
        }
        let _ = display.flush_clients();
        let _ = done.send(());
    }))
    .unwrap();
    finished.recv_timeout(Duration::from_secs(60)).unwrap();
}

// ---- the client ----

mod client {
    use wayland_client::globals::GlobalListContents;
    use wayland_client::protocol::{wl_compositor, wl_pointer, wl_registry, wl_seat, wl_surface};
    use wayland_client::{Connection, Dispatch, QueueHandle};

    #[derive(Default)]
    pub struct State {
        pub motions: usize,
        pub last: Option<(f64, f64)>,
    }

    macro_rules! quiet {
        ($($iface:ty),*) => {$(
            impl Dispatch<$iface, ()> for State {
                fn event(_: &mut Self, _: &$iface, _: <$iface as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
            }
        )*};
    }
    quiet!(wl_compositor::WlCompositor, wl_surface::WlSurface, wl_seat::WlSeat);

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wl_pointer::WlPointer, ()> for State {
        fn event(state: &mut Self, _: &wl_pointer::WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let wl_pointer::Event::Motion { surface_x, surface_y, .. } = event {
                state.motions += 1;
                state.last = Some((surface_x, surface_y));
            }
        }
    }
}

use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_compositor, wl_pointer, wl_seat, wl_surface};
use wayland_client::{Connection, EventQueue};

struct App {
    queue: EventQueue<client::State>,
    state: client::State,
    _surface: wl_surface::WlSurface,
    _pointer: wl_pointer::WlPointer,
}

impl App {
    fn new(stream: UnixStream) -> Self {
        let conn = Connection::from_socket(stream).unwrap();
        let (globals, mut queue) = registry_queue_init::<client::State>(&conn).unwrap();
        let qh = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=6, ()).unwrap();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
        let surface = compositor.create_surface(&qh, ());
        let pointer = seat.get_pointer(&qh, ());
        let mut state = client::State::default();
        queue.roundtrip(&mut state).unwrap();
        Self { queue, state, _surface: surface, _pointer: pointer }
    }

    /// Read everything the server sent; false if it disconnected us.
    fn catch_up(&mut self) -> bool {
        self.queue.roundtrip(&mut self.state).is_ok()
    }
}

/// Ten seconds of motion (about 280 KB of events) while not reading: more
/// than a default socket and library buffer hold.
const STALL: u32 = 10;

#[test]
fn an_ordinary_client_that_stops_reading_is_dropped() {
    let (server_end, client_end) = UnixStream::pair().unwrap();
    let (tx, gone) = start_server(server_end, false);
    let mut app = App::new(client_end);
    // The client stalls (it does not read) while the mouse moves.
    // (Three times as long: whatever the machine's default socket size.)
    flood(&tx, 3 * STALL);
    let why = gone.lock().unwrap().clone();
    let why = why.expect("dropped");
    assert!(why.starts_with("connection closed with ") && why.ends_with("(it stopped reading its socket)"), "dropped for not reading: {why}");
    assert!(!app.catch_up(), "the connection is gone");
}

#[test]
fn the_shell_that_stops_reading_keeps_its_connection() {
    let (server_end, client_end) = UnixStream::pair().unwrap();
    let (tx, gone) = start_server(server_end, true);
    let mut app = App::new(client_end);
    flood(&tx, STALL);
    // Give the server a few loop turns with the backlog.
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(gone.lock().unwrap().clone(), None, "the shell was dropped");
    // It reads again: every motion arrives (the first was the enter),
    // the last one last.
    assert!(app.catch_up(), "the connection survived");
    assert!(app.catch_up());
    assert_eq!(app.state.motions, (STALL * 1000 - 1) as usize);
    let last = STALL * 1000 - 1;
    assert_eq!(app.state.last, Some((f64::from(last % 500), f64::from(last % 300))));
    assert_eq!(gone.lock().unwrap().clone(), None);
}
