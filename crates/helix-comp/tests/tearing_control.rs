//! `wp_tearing_control_v1` (helix-desktop-spec `contracts/wayland/README.md`,
//! "Tearing") between Helix's server side and a real Wayland client,
//! in-process: the hint is per surface, applied by the surface's commit,
//! back to `vsync` when the control object is destroyed, a second control
//! object for a surface is a protocol error, and a control object whose
//! surface is gone is inert.

use std::os::unix::net::UnixStream;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use helix_comp::tearing_control::{current_hint, ControlData, Hint, TearingControl};
use smithay::reexports::wayland_protocols::wp::tearing_control::v1::server as tearing_server;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, Resource};
use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState};
use smithay::delegate_compositor;

// ---- the server ----

struct Server {
    compositor: CompositorState,
    surfaces: Vec<WlSurface>,
}

#[derive(Default)]
struct ClientInfo {
    compositor: CompositorClientState,
}

impl ClientData for ClientInfo {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
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
smithay::reexports::wayland_server::delegate_global_dispatch!(Server: [tearing_server::wp_tearing_control_manager_v1::WpTearingControlManagerV1: ()] => TearingControl);
smithay::reexports::wayland_server::delegate_dispatch!(Server: [tearing_server::wp_tearing_control_manager_v1::WpTearingControlManagerV1: ()] => TearingControl);
smithay::reexports::wayland_server::delegate_dispatch!(Server: [tearing_server::wp_tearing_control_v1::WpTearingControlV1: ControlData] => TearingControl);

type Command = Box<dyn FnOnce(&mut Server) + Send>;

fn start_server(stream: UnixStream) -> mpsc::Sender<Command> {
    let (tx, rx) = mpsc::channel::<Command>();
    std::thread::spawn(move || {
        let mut display: Display<Server> = Display::new().unwrap();
        let dh: DisplayHandle = display.handle();
        let mut state = Server { compositor: CompositorState::new::<Server>(&dh), surfaces: Vec::new() };
        helix_comp::tearing_control::create_global::<Server>(&dh);
        display.handle().insert_client(stream, Arc::new(ClientInfo::default())).unwrap();
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
    tx
}

/// The committed hints of the server's live surfaces, in creation order.
fn hints(tx: &mpsc::Sender<Command>) -> Vec<Hint> {
    let (reply, answer) = mpsc::channel();
    tx.send(Box::new(move |state: &mut Server| {
        state.surfaces.retain(|s| s.is_alive());
        let _ = reply.send(state.surfaces.iter().map(current_hint).collect());
    }))
    .unwrap();
    answer.recv_timeout(Duration::from_secs(5)).unwrap()
}

// ---- the client ----

mod client {
    use wayland_client::globals::GlobalListContents;
    use wayland_client::protocol::{wl_compositor, wl_registry, wl_surface};
    use wayland_client::{Connection, Dispatch, QueueHandle};
    use wayland_protocols::wp::tearing_control::v1::client::{wp_tearing_control_manager_v1, wp_tearing_control_v1};

    #[derive(Default)]
    pub struct State;

    macro_rules! quiet {
        ($($iface:ty),*) => {$(
            impl Dispatch<$iface, ()> for State {
                fn event(_: &mut Self, _: &$iface, _: <$iface as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
            }
        )*};
    }
    quiet!(wl_compositor::WlCompositor, wl_surface::WlSurface, wp_tearing_control_manager_v1::WpTearingControlManagerV1, wp_tearing_control_v1::WpTearingControlV1);

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
    }
}

use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_compositor;
use wayland_client::{Connection, EventQueue, QueueHandle};
use wayland_protocols::wp::tearing_control::v1::client::wp_tearing_control_manager_v1::WpTearingControlManagerV1;
use wayland_protocols::wp::tearing_control::v1::client::wp_tearing_control_v1::PresentationHint;

struct App {
    conn: Connection,
    queue: EventQueue<client::State>,
    state: client::State,
    qh: QueueHandle<client::State>,
    compositor: wl_compositor::WlCompositor,
    manager: WpTearingControlManagerV1,
}

impl App {
    fn new(stream: UnixStream) -> Self {
        let conn = Connection::from_socket(stream).unwrap();
        let (globals, mut queue) = registry_queue_init::<client::State>(&conn).unwrap();
        let qh = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=6, ()).unwrap();
        let manager: WpTearingControlManagerV1 = globals.bind(&qh, 1..=1, ()).expect("the tearing control global");
        let mut state = client::State;
        queue.roundtrip(&mut state).unwrap();
        Self { conn, queue, state, qh, compositor, manager }
    }

    fn sync(&mut self) -> bool {
        self.queue.roundtrip(&mut self.state).is_ok()
    }

    fn error_code(&mut self) -> Option<u32> {
        self.conn.flush().ok()?;
        while self.queue.blocking_dispatch(&mut self.state).is_ok() {}
        self.conn.protocol_error().map(|e| e.code)
    }
}

fn pair() -> (App, mpsc::Sender<Command>) {
    let (a, b) = UnixStream::pair().unwrap();
    let tx = start_server(a);
    (App::new(b), tx)
}

#[test]
fn hint_per_surface_applied_on_commit() {
    let (mut app, tx) = pair();
    let game = app.compositor.create_surface(&app.qh, ());
    let other = app.compositor.create_surface(&app.qh, ());
    assert!(app.sync());
    assert_eq!(hints(&tx), [Hint::Vsync, Hint::Vsync], "vsync by default");

    let control = app.manager.get_tearing_control(&game, &app.qh, ());
    control.set_presentation_hint(PresentationHint::Async);
    assert!(app.sync());
    assert_eq!(hints(&tx), [Hint::Vsync, Hint::Vsync], "not before the commit");
    game.commit();
    assert!(app.sync());
    assert_eq!(hints(&tx), [Hint::Async, Hint::Vsync], "only that surface");

    control.set_presentation_hint(PresentationHint::Vsync);
    game.commit();
    assert!(app.sync());
    assert_eq!(hints(&tx), [Hint::Vsync, Hint::Vsync]);

    // Destroying the control object puts vsync back at the next commit.
    control.set_presentation_hint(PresentationHint::Async);
    game.commit();
    assert!(app.sync());
    control.destroy();
    assert!(app.sync());
    assert_eq!(hints(&tx), [Hint::Async, Hint::Vsync]);
    game.commit();
    assert!(app.sync());
    assert_eq!(hints(&tx), [Hint::Vsync, Hint::Vsync], "reset when the control object goes");

    // A new control object may be made after the old one is gone.
    let again = app.manager.get_tearing_control(&game, &app.qh, ());
    again.set_presentation_hint(PresentationHint::Async);
    game.commit();
    assert!(app.sync(), "a second control after the first was destroyed is fine");
    assert_eq!(hints(&tx), [Hint::Async, Hint::Vsync]);

    // The surface goes: its control object is inert, no error.
    game.destroy();
    assert!(app.sync());
    again.set_presentation_hint(PresentationHint::Vsync);
    again.destroy();
    assert!(app.sync(), "requests on an orphaned control object are ignored");
    assert_eq!(hints(&tx), [Hint::Vsync], "the other surface is left");
    drop(other);
    let _ = app.conn.flush();
}

#[test]
fn a_second_control_object_is_an_error() {
    let (mut app, _tx) = pair();
    let surface = app.compositor.create_surface(&app.qh, ());
    let _first = app.manager.get_tearing_control(&surface, &app.qh, ());
    assert!(app.sync());
    let _second = app.manager.get_tearing_control(&surface, &app.qh, ());
    assert_eq!(app.error_code(), Some(0), "tearing_control_exists");
}
