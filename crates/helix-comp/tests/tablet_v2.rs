//! `zwp_tablet_manager_v2` (helix-desktop-spec
//! `contracts/gsettings/input-settings.md`, "Tablets") between Helix's
//! server side and a real Wayland client, in-process. No tablet hardware
//! is involved: the test plays the compositor and tells the server about
//! devices and input.

use std::os::unix::net::UnixStream;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use helix_comp::tablet_v2::{Axes, PadGroup, PadInfo, TabletInfo, TabletV2, TabletV2Handler, ToolCaps, ToolInfo};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_protocols::wp::tablet::zv2::server as s;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, Resource};
use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState};
use smithay::{delegate_compositor, delegate_seat};

// ---- the server ----

struct Server {
    compositor: CompositorState,
    seat_state: SeatState<Server>,
    _seat: Seat<Server>,
    tablet: TabletV2,
    surfaces: Vec<WlSurface>,
    cursors: Arc<Mutex<Vec<(u64, bool)>>>,
    shapes: Arc<Mutex<Vec<String>>>,
}

static SHAPES: std::sync::LazyLock<Arc<Mutex<Vec<String>>>> = std::sync::LazyLock::new(Default::default);

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

impl SeatHandler for Server {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
}

impl TabletV2Handler for Server {
    fn tablet_v2(&mut self) -> &mut TabletV2 {
        &mut self.tablet
    }
    fn tool_cursor(&mut self, tool: u64, cursor: Option<(WlSurface, (i32, i32))>) {
        self.cursors.lock().unwrap().push((tool, cursor.is_some()));
    }
}

impl helix_comp::cursor_shape::CursorShapeHandler for Server {
    fn pointer_shape(&mut self, _: ClientId, icon: smithay::input::pointer::CursorIcon) {
        self.shapes.lock().unwrap().push(format!("pointer {icon:?}"));
    }
    fn tool_shape(&mut self, _: ClientId, tool: u64, icon: smithay::input::pointer::CursorIcon) {
        self.shapes.lock().unwrap().push(format!("tool {tool} {icon:?}"));
    }
}

delegate_compositor!(Server);
delegate_seat!(Server);
smithay::reexports::wayland_server::delegate_global_dispatch!(Server: [s::zwp_tablet_manager_v2::ZwpTabletManagerV2: ()] => TabletV2);
smithay::reexports::wayland_server::delegate_dispatch!(Server: [s::zwp_tablet_manager_v2::ZwpTabletManagerV2: ()] => TabletV2);
smithay::reexports::wayland_server::delegate_dispatch!(Server: [s::zwp_tablet_seat_v2::ZwpTabletSeatV2: ()] => TabletV2);
smithay::reexports::wayland_server::delegate_dispatch!(Server: [s::zwp_tablet_v2::ZwpTabletV2: String] => TabletV2);
smithay::reexports::wayland_server::delegate_dispatch!(Server: [s::zwp_tablet_tool_v2::ZwpTabletToolV2: u64] => TabletV2);
smithay::reexports::wayland_server::delegate_dispatch!(Server: [s::zwp_tablet_pad_v2::ZwpTabletPadV2: String] => TabletV2);
smithay::reexports::wayland_server::delegate_dispatch!(Server: [s::zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2: (String, usize)] => TabletV2);
smithay::reexports::wayland_server::delegate_dispatch!(Server: [s::zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2: (String, u32)] => TabletV2);
smithay::reexports::wayland_server::delegate_dispatch!(Server: [s::zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2: (String, u32)] => TabletV2);
use smithay::reexports::wayland_protocols::wp::cursor_shape::v1::server as cs;
smithay::reexports::wayland_server::delegate_global_dispatch!(Server: [cs::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1: ()] => helix_comp::cursor_shape::CursorShapeManager);
smithay::reexports::wayland_server::delegate_dispatch!(Server: [cs::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1: ()] => helix_comp::cursor_shape::CursorShapeManager);
smithay::reexports::wayland_server::delegate_dispatch!(Server: [cs::wp_cursor_shape_device_v1::WpCursorShapeDeviceV1: helix_comp::cursor_shape::Device] => helix_comp::cursor_shape::CursorShapeManager);

type Command = Box<dyn FnOnce(&mut Server, &DisplayHandle) + Send>;

/// Run the server on its own thread for `streams` clients.
fn start_server(streams: Vec<UnixStream>, cursors: Arc<Mutex<Vec<(u64, bool)>>>) -> mpsc::Sender<Command> {
    let (tx, rx) = mpsc::channel::<Command>();
    std::thread::spawn(move || {
        let mut display: Display<Server> = Display::new().unwrap();
        let dh = display.handle();
        let compositor = CompositorState::new::<Server>(&dh);
        let mut seat_state = SeatState::new();
        let seat = seat_state.new_wl_seat(&dh, "seat0");
        TabletV2::global::<Server>(&dh);
        let _shapes = helix_comp::cursor_shape::CursorShapeManager::new::<Server>(&dh);
        let mut state = Server { compositor, seat_state, _seat: seat, tablet: TabletV2::default(), surfaces: Vec::new(), cursors, shapes: SHAPES.clone() };
        for stream in streams {
            display.handle().insert_client(stream, Arc::new(ClientInfo::default())).unwrap();
        }
        loop {
            loop {
                match rx.try_recv() {
                    Ok(command) => command(&mut state, &dh),
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

/// Run `f` on the server and wait for its answer.
fn on_server<T: Send + 'static>(tx: &mpsc::Sender<Command>, f: impl FnOnce(&mut Server, &DisplayHandle) -> T + Send + 'static) -> T {
    let (reply, answer) = mpsc::channel();
    tx.send(Box::new(move |state, dh| {
        let _ = reply.send(f(state, dh));
    }))
    .unwrap();
    answer.recv_timeout(Duration::from_secs(5)).unwrap()
}

// ---- the client ----

mod client {
    use wayland_client::globals::GlobalListContents;
    use wayland_client::protocol::{wl_compositor, wl_registry, wl_seat, wl_surface};
    use wayland_client::{event_created_child, Connection, Dispatch, QueueHandle};
    use wayland_protocols::wp::tablet::zv2::client::*;
    use wayland_protocols::wp::cursor_shape::v1::client::{wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1};

    impl Dispatch<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1, ()> for Log {
        fn event(_: &mut Self, _: &wp_cursor_shape_manager_v1::WpCursorShapeManagerV1, _: wp_cursor_shape_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1, ()> for Log {
        fn event(_: &mut Self, _: &wp_cursor_shape_device_v1::WpCursorShapeDeviceV1, _: wp_cursor_shape_device_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }

    #[derive(Default)]
    pub struct Log(pub Vec<String>, pub Vec<zwp_tablet_tool_v2::ZwpTabletToolV2>, pub Vec<zwp_tablet_pad_v2::ZwpTabletPadV2>);

    impl Log {
        fn push(&mut self, s: String) {
            self.0.push(s);
        }
    }

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Log {
        fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wl_compositor::WlCompositor, ()> for Log {
        fn event(_: &mut Self, _: &wl_compositor::WlCompositor, _: wl_compositor::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wl_surface::WlSurface, ()> for Log {
        fn event(_: &mut Self, _: &wl_surface::WlSurface, _: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<wl_seat::WlSeat, ()> for Log {
        fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<zwp_tablet_manager_v2::ZwpTabletManagerV2, ()> for Log {
        fn event(_: &mut Self, _: &zwp_tablet_manager_v2::ZwpTabletManagerV2, _: zwp_tablet_manager_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<zwp_tablet_seat_v2::ZwpTabletSeatV2, ()> for Log {
        fn event(log: &mut Self, _: &zwp_tablet_seat_v2::ZwpTabletSeatV2, event: zwp_tablet_seat_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            let what = match event {
                zwp_tablet_seat_v2::Event::TabletAdded { .. } => "tablet_added",
                zwp_tablet_seat_v2::Event::ToolAdded { id } => {
                    log.1.push(id);
                    "tool_added"
                }
                zwp_tablet_seat_v2::Event::PadAdded { id } => {
                    log.2.push(id);
                    "pad_added"
                }
                _ => "seat?",
            };
            log.push(what.into());
        }
        event_created_child!(Log, zwp_tablet_seat_v2::ZwpTabletSeatV2, [
            zwp_tablet_seat_v2::EVT_TABLET_ADDED_OPCODE => (zwp_tablet_v2::ZwpTabletV2, ()),
            zwp_tablet_seat_v2::EVT_TOOL_ADDED_OPCODE => (zwp_tablet_tool_v2::ZwpTabletToolV2, ()),
            zwp_tablet_seat_v2::EVT_PAD_ADDED_OPCODE => (zwp_tablet_pad_v2::ZwpTabletPadV2, ()),
        ]);
    }
    impl Dispatch<zwp_tablet_v2::ZwpTabletV2, ()> for Log {
        fn event(log: &mut Self, _: &zwp_tablet_v2::ZwpTabletV2, event: zwp_tablet_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            log.push(match event {
                zwp_tablet_v2::Event::Name { name } => format!("tablet name {name}"),
                zwp_tablet_v2::Event::Id { vid, pid } => format!("tablet id {vid:04x}:{pid:04x}"),
                zwp_tablet_v2::Event::Path { path } => format!("tablet path {path}"),
                zwp_tablet_v2::Event::Done => "tablet done".into(),
                zwp_tablet_v2::Event::Removed => "tablet removed".into(),
                e => format!("tablet {e:?}"),
            });
        }
    }
    impl Dispatch<zwp_tablet_tool_v2::ZwpTabletToolV2, ()> for Log {
        fn event(log: &mut Self, _: &zwp_tablet_tool_v2::ZwpTabletToolV2, event: zwp_tablet_tool_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            use zwp_tablet_tool_v2::Event as E;
            log.push(match event {
                E::Type { tool_type } => format!("tool type {:?}", tool_type.into_result().unwrap()),
                E::HardwareSerial { hardware_serial_hi, hardware_serial_lo } => format!("tool serial {hardware_serial_hi:x}:{hardware_serial_lo:x}"),
                E::HardwareIdWacom { hardware_id_hi, hardware_id_lo } => format!("tool hwid {hardware_id_hi:x}:{hardware_id_lo:x}"),
                E::Capability { capability } => format!("tool cap {:?}", capability.into_result().unwrap()),
                E::Done => "tool done".into(),
                E::Removed => "tool removed".into(),
                E::ProximityIn { serial, .. } => format!("proximity_in {serial}"),
                E::ProximityOut => "proximity_out".into(),
                E::Down { serial } => format!("down {serial}"),
                E::Up => "up".into(),
                E::Motion { x, y } => format!("motion {x} {y}"),
                E::Pressure { pressure } => format!("pressure {pressure}"),
                E::Distance { distance } => format!("distance {distance}"),
                E::Tilt { tilt_x, tilt_y } => format!("tilt {tilt_x} {tilt_y}"),
                E::Rotation { degrees } => format!("rotation {degrees}"),
                E::Slider { position } => format!("slider {position}"),
                E::Wheel { degrees, clicks } => format!("wheel {degrees} {clicks}"),
                E::Button { serial, button, state } => format!("button {serial} {button:#x} {:?}", state.into_result().unwrap()),
                E::Frame { time } => format!("frame {time}"),
                e => format!("tool {e:?}"),
            });
        }
    }
    impl Dispatch<zwp_tablet_pad_v2::ZwpTabletPadV2, ()> for Log {
        fn event(log: &mut Self, _: &zwp_tablet_pad_v2::ZwpTabletPadV2, event: zwp_tablet_pad_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            use zwp_tablet_pad_v2::Event as E;
            log.push(match event {
                E::Group { .. } => "pad group".into(),
                E::Path { path } => format!("pad path {path}"),
                E::Buttons { buttons } => format!("pad buttons {buttons}"),
                E::Done => "pad done".into(),
                E::Button { time, button, state } => format!("pad button {time} {button} {:?}", state.into_result().unwrap()),
                E::Enter { serial, .. } => format!("pad enter {serial}"),
                E::Leave { serial, .. } => format!("pad leave {serial}"),
                E::Removed => "pad removed".into(),
                e => format!("pad {e:?}"),
            });
        }
        event_created_child!(Log, zwp_tablet_pad_v2::ZwpTabletPadV2, [
            zwp_tablet_pad_v2::EVT_GROUP_OPCODE => (zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, ()),
        ]);
    }
    impl Dispatch<zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, ()> for Log {
        fn event(log: &mut Self, _: &zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, event: zwp_tablet_pad_group_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            use zwp_tablet_pad_group_v2::Event as E;
            log.push(match event {
                E::Buttons { buttons } => {
                    let list: Vec<u32> = buttons.chunks(4).map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]])).collect();
                    format!("group buttons {list:?}")
                }
                E::Ring { .. } => "group ring".into(),
                E::Strip { .. } => "group strip".into(),
                E::Modes { modes } => format!("group modes {modes}"),
                E::Done => "group done".into(),
                E::ModeSwitch { time, serial, mode } => format!("mode_switch {time} {serial} {mode}"),
                e => format!("group {e:?}"),
            });
        }
        event_created_child!(Log, zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, [
            zwp_tablet_pad_group_v2::EVT_RING_OPCODE => (zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2, ()),
            zwp_tablet_pad_group_v2::EVT_STRIP_OPCODE => (zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2, ()),
        ]);
    }
    impl Dispatch<zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2, ()> for Log {
        fn event(log: &mut Self, _: &zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2, event: zwp_tablet_pad_ring_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            use zwp_tablet_pad_ring_v2::Event as E;
            log.push(match event {
                E::Source { source } => format!("ring source {:?}", source.into_result().unwrap()),
                E::Angle { degrees } => format!("ring angle {degrees}"),
                E::Stop => "ring stop".into(),
                E::Frame { time } => format!("ring frame {time}"),
                e => format!("ring {e:?}"),
            });
        }
    }
    impl Dispatch<zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2, ()> for Log {
        fn event(log: &mut Self, _: &zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2, event: zwp_tablet_pad_strip_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            use zwp_tablet_pad_strip_v2::Event as E;
            log.push(match event {
                E::Source { source } => format!("strip source {:?}", source.into_result().unwrap()),
                E::Position { position } => format!("strip position {position}"),
                E::Stop => "strip stop".into(),
                E::Frame { time } => format!("strip frame {time}"),
                e => format!("strip {e:?}"),
            });
        }
    }
}

use client::Log;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_compositor, wl_seat};
use wayland_client::{Connection, EventQueue};
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_manager_v2;

struct App {
    conn: Connection,
    queue: EventQueue<Log>,
    log: Log,
    surface: wayland_client::protocol::wl_surface::WlSurface,
    seat: wl_seat::WlSeat,
    manager: Option<zwp_tablet_manager_v2::ZwpTabletManagerV2>,
}

impl App {
    fn new(stream: UnixStream) -> Self {
        let conn = Connection::from_socket(stream).unwrap();
        let (globals, queue) = registry_queue_init::<Log>(&conn).unwrap();
        let qh = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=4, ()).unwrap();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).unwrap();
        let manager = globals.bind::<zwp_tablet_manager_v2::ZwpTabletManagerV2, _, _>(&qh, 1..=1, ()).ok();
        let surface = compositor.create_surface(&qh, ());
        let mut app = Self { conn: conn.clone(), queue, log: Log::default(), surface, seat, manager };
        app.sync();
        app
    }

    fn queue_conn(&self) -> Connection {
        self.conn.clone()
    }

    fn sync(&mut self) {
        self.queue.roundtrip(&mut self.log).unwrap();
    }

    /// The events since the last call.
    fn take(&mut self) -> Vec<String> {
        self.sync();
        std::mem::take(&mut self.log.0)
    }
}

fn tablet_info() -> TabletInfo {
    TabletInfo { id: "event7".into(), name: "Wacom Intuos Pro M Pen".into(), usb: Some((0x56a, 0x357)), path: "/dev/input/event7".into() }
}

fn tool_info() -> ToolInfo {
    ToolInfo {
        id: 42,
        kind: s::zwp_tablet_tool_v2::Type::Pen,
        serial: 0x1234_5678_9abc,
        hardware_id: 0x802,
        caps: ToolCaps { tilt: true, pressure: true, distance: true, ..ToolCaps::default() },
    }
}

fn pad_info() -> PadInfo {
    PadInfo {
        id: "event8".into(),
        tablet: "event7".into(),
        path: "/dev/input/event8".into(),
        buttons: 3,
        groups: vec![PadGroup { buttons: vec![0, 1, 2], rings: vec![0], strips: vec![0], modes: 2, mode: 0 }],
    }
}

#[test]
fn tablets_tools_and_pads() {
    let (server_end, client_end) = UnixStream::pair().unwrap();
    let (server_end2, client_end2) = UnixStream::pair().unwrap();
    let cursors = Arc::new(Mutex::new(Vec::new()));
    let server = start_server(vec![server_end, server_end2], cursors.clone());
    let mut app = App::new(client_end);
    let mut plain = App::new(client_end2);

    // A tablet known before the client asks: announced when it does.
    on_server(&server, |state, dh| state.tablet.add_tablet::<Server>(dh, tablet_info()));
    let qh = app.queue.handle();
    let _tablet_seat = app.manager.as_ref().expect("the tablet global").get_tablet_seat(&app.seat, &qh, ());
    assert_eq!(app.take(), ["tablet_added", "tablet name Wacom Intuos Pro M Pen", "tablet id 056a:0357", "tablet path /dev/input/event7", "tablet done"]);

    // A tool and a pad added later: announced at once.
    on_server(&server, |state, dh| {
        state.tablet.add_tool::<Server>(dh, tool_info());
        state.tablet.add_pad::<Server>(dh, pad_info());
    });
    assert_eq!(
        app.take(),
        [
            "tool_added", "tool type Pen", "tool serial 1234:56789abc", "tool hwid 0:802", "tool cap Tilt", "tool cap Pressure", "tool cap Distance", "tool done",
            "pad_added", "pad group", "group buttons [0, 1, 2]", "group ring", "group strip", "group modes 2", "group done", "pad path /dev/input/event8", "pad buttons 3", "pad done",
        ]
    );

    // The pen over the client's surface.
    let got = on_server(&server, |state, _| {
        let surface = state.surfaces[0].clone();
        let over = state.tablet.tool_over(42, "event7", Some((&surface, (10.5, 20.25))), 5, 100);
        state.tablet.tool_axes(42, &Axes { pressure: Some(0.5), tilt: Some((10.0, -5.0)), distance: Some(0.0), ..Axes::default() });
        state.tablet.tool_tip(42, true, 6);
        state.tablet.tool_button(42, 0x14b, true, 7);
        state.tablet.tool_frame(42, 100);
        over
    });
    assert!(got, "the client gets tablet events");
    assert_eq!(app.take(), ["proximity_in 5", "motion 10.5 20.25", "pressure 32768", "distance 0", "tilt 10 -5", "down 6", "button 7 0x14b Pressed", "frame 100"]);

    // The client sets the tool's cursor, then hides it.
    let cursor = app.surface.clone();
    app.log.1[0].set_cursor(5, Some(&cursor), 1, 2);
    app.log.1[0].set_cursor(5, None, 0, 0);
    app.sync();
    assert_eq!(*cursors.lock().unwrap(), [(42, true), (42, false)]);

    // Or names a shape for it (wp_cursor_shape_device_v1).
    {
        use wayland_protocols::wp::cursor_shape::v1::client::{wp_cursor_shape_device_v1::Shape, wp_cursor_shape_manager_v1::WpCursorShapeManagerV1};
        let qh = app.queue.handle();
        let (globals, _) = wayland_client::globals::registry_queue_init::<Log>(&app.queue_conn()).unwrap();
        let manager: WpCursorShapeManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
        let device = manager.get_tablet_tool_v2(&app.log.1[0], &qh, ());
        device.set_shape(5, Shape::Text);
        device.set_shape(5, Shape::Crosshair);
        app.sync();
        app.sync();
    }
    assert_eq!(*SHAPES.lock().unwrap(), ["tool 42 Text", "tool 42 Crosshair"]);

    on_server(&server, |state, _| state.tablet.tool_over(42, "event7", None, 8, 110));
    assert_eq!(app.take(), ["proximity_out", "frame 110"]);

    // A client without a tablet seat: the compositor treats the pen as a
    // pointer there.
    let over_plain = on_server(&server, |state, _| {
        let surface = state.surfaces.iter().find(|s| s.client().map(|c| c.id()) != state.surfaces[0].client().map(|c| c.id())).cloned().unwrap();
        state.tablet.tool_over(42, "event7", Some((&surface, (1.0, 1.0))), 9, 120)
    });
    assert!(!over_plain);
    assert!(plain.take().is_empty());
    assert!(app.take().is_empty(), "nothing for the first client either");

    // The pad follows the keyboard focus to the client's surface.
    on_server(&server, |state, _| {
        let surface = state.surfaces[0].clone();
        state.tablet.pad_focus("event8", Some(&surface), 10);
        state.tablet.pad_button("event8", 200, 1, true);
        state.tablet.pad_ring("event8", 0, Some(90.0), true, 201);
        state.tablet.pad_ring("event8", 0, None, false, 202);
        state.tablet.pad_strip("event8", 0, Some(0.5), true, 203);
        state.tablet.pad_mode("event8", 0, 1, 204, 11);
        state.tablet.pad_mode("event8", 0, 1, 205, 12);
    });
    assert_eq!(
        app.take(),
        [
            "pad enter 10", "mode_switch 0 10 0",
            "pad button 200 1 Pressed",
            "ring source Finger", "ring angle 90", "ring frame 201",
            "ring stop", "ring frame 202",
            "strip source Finger", "strip position 32768", "strip frame 203",
            "mode_switch 204 11 1",
        ],
        "the same mode twice is sent once"
    );
    // The focused app says what a button does; the compositor keeps it
    // until the focus leaves the app.
    let pad = app.log.2[0].clone();
    pad.set_feedback(1, "Undo".into(), 10);
    pad.set_feedback(2, "Redo".into(), 10);
    pad.set_feedback(2, "".into(), 10);
    app.sync();
    let said = on_server(&server, |state, _| state.tablet.feedback("event8"));
    assert_eq!(said, [(helix_comp::tablet_v2::Control::Button(1), "Undo".to_owned())], "an empty description withdraws one");
    on_server(&server, |state, _| state.tablet.pad_focus("event8", None, 13));
    assert!(on_server(&server, |state, _| state.tablet.feedback("event8")).is_empty());
    assert_eq!(app.take(), ["pad leave 13"]);

    // Devices going away.
    on_server(&server, |state, _| {
        state.tablet.remove_tool(42);
        state.tablet.remove_pad("event8");
        state.tablet.remove_tablet("event7");
    });
    assert_eq!(app.take(), ["tool removed", "pad removed", "tablet removed"]);
    drop(plain);
}
