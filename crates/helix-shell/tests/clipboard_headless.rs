//! The shell's text fields and the real Wayland clipboard, end to end: a
//! headless Helix session (helix-comp with helix-shell, scratch home,
//! private bus) and a Wayland client window (this test).
//!
//! - The client copies; the launcher's search field pastes it (Ctrl+V).
//! - The launcher copies (Ctrl+C); the client pastes what it copied.
//! - The client owns the primary selection; a middle click on the
//!   launcher pastes it.
//!
//! Needs helix-comp built next to this test (`cargo test --workspace`
//! builds it); skipped otherwise, or without dbus-daemon.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};
use smithay_client_toolkit::reexports::client::globals::{registry_queue_init, GlobalListContents};
use smithay_client_toolkit::reexports::client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use smithay_client_toolkit::reexports::client::{event_created_child, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use smithay_client_toolkit::reexports::protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1 as psm, zwp_primary_selection_device_v1 as psd, zwp_primary_selection_offer_v1 as pso, zwp_primary_selection_source_v1 as pss,
};
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const TEXT: &str = "text/plain;charset=utf-8";
const SIZE: (u32, u32) = (1024, 640);

/// The mime types an offer has.
#[derive(Default)]
struct OfferData(Mutex<Vec<String>>);

#[derive(Default)]
struct Client {
    configure: Option<u32>,
    /// A frame was shown (the window is mapped).
    shown: bool,
    focused: bool,
    /// The last input serial (pointer button or key).
    serial: Option<u32>,
    selection: Option<(wl_data_offer::WlDataOffer, Vec<String>)>,
    cancelled: VecDeque<&'static str>,
    /// What our clipboard and primary sources serve.
    clipboard_text: String,
    primary_text: String,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Client {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

macro_rules! quiet {
    ($($iface:ty),*) => {$(
        impl Dispatch<$iface, ()> for Client {
            fn event(_: &mut Self, _: &$iface, _: <$iface as smithay_client_toolkit::reexports::client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
quiet!(wl_compositor::WlCompositor, wl_surface::WlSurface, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, wl_seat::WlSeat, wl_data_device_manager::WlDataDeviceManager, psm::ZwpPrimarySelectionDeviceManagerV1, xdg_toplevel::XdgToplevel);

impl Dispatch<wl_callback::WlCallback, ()> for Client {
    fn event(state: &mut Self, _: &wl_callback::WlCallback, event: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = event {
            state.shown = true;
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Client {
    fn event(_: &mut Self, base: &xdg_wm_base::XdgWmBase, event: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Client {
    fn event(state: &mut Self, _: &xdg_surface::XdgSurface, event: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = event {
            state.configure = Some(serial);
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Client {
    fn event(state: &mut Self, _: &wl_pointer::WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_pointer::Event::Button { serial, .. } = event {
            state.serial = Some(serial);
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Client {
    fn event(state: &mut Self, _: &wl_keyboard::WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_keyboard::Event::Enter { .. } => state.focused = true,
            wl_keyboard::Event::Leave { .. } => state.focused = false,
            wl_keyboard::Event::Key { serial, .. } => state.serial = Some(serial),
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for Client {
    fn event(state: &mut Self, _: &wl_data_device::WlDataDevice, event: wl_data_device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_data_device::Event::Selection { id } = event {
            state.selection = id.map(|offer| {
                let types = offer.data::<OfferData>().map(|d| d.0.lock().unwrap().clone()).unwrap_or_default();
                (offer, types)
            });
        }
    }
    event_created_child!(Client, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, OfferData::default()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, OfferData> for Client {
    fn event(_: &mut Self, _: &wl_data_offer::WlDataOffer, event: wl_data_offer::Event, data: &OfferData, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            data.0.lock().unwrap().push(mime_type);
        }
    }
}

fn serve(fd: OwnedFd, text: &str) {
    let mut file = std::fs::File::from(fd);
    let _ = file.write_all(text.as_bytes());
}

impl Dispatch<wl_data_source::WlDataSource, ()> for Client {
    fn event(state: &mut Self, _: &wl_data_source::WlDataSource, event: wl_data_source::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_data_source::Event::Send { fd, .. } => serve(fd, &state.clipboard_text),
            wl_data_source::Event::Cancelled => state.cancelled.push_back("clipboard"),
            _ => {}
        }
    }
}

impl Dispatch<psd::ZwpPrimarySelectionDeviceV1, ()> for Client {
    fn event(_: &mut Self, _: &psd::ZwpPrimarySelectionDeviceV1, _: psd::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    event_created_child!(Client, psd::ZwpPrimarySelectionDeviceV1, [
        psd::EVT_DATA_OFFER_OPCODE => (pso::ZwpPrimarySelectionOfferV1, ()),
    ]);
}
quiet!(pso::ZwpPrimarySelectionOfferV1);

impl Dispatch<pss::ZwpPrimarySelectionSourceV1, ()> for Client {
    fn event(state: &mut Self, _: &pss::ZwpPrimarySelectionSourceV1, event: pss::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            pss::Event::Send { fd, .. } => serve(fd, &state.primary_text),
            pss::Event::Cancelled => state.cancelled.push_back("primary"),
            _ => {}
        }
    }
}

struct App {
    conn: Connection,
    queue: EventQueue<Client>,
    state: Client,
}

impl App {
    /// Dispatch what comes within `ms`.
    fn pump(&mut self, ms: u64) {
        self.queue.dispatch_pending(&mut self.state).unwrap();
        self.queue.flush().unwrap();
        if let Some(guard) = self.queue.prepare_read() {
            let mut fds = [libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 }];
            // SAFETY: one valid pollfd.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, ms as i32) };
            if ready > 0 {
                let _ = guard.read();
            }
        }
        self.queue.dispatch_pending(&mut self.state).unwrap();
        self.queue.flush().unwrap();
    }

    /// Dispatch for `ms` milliseconds.
    fn pump_for(&mut self, ms: u64) {
        let until = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < until {
            self.pump(20);
        }
    }

    /// Dispatch until `done`, or fail with `what` after 10 s.
    fn until(&mut self, what: &str, done: impl Fn(&Client) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(&self.state) {
            assert!(Instant::now() < deadline, "timed out: {what}");
            self.pump(50);
        }
    }

    /// Read the current clipboard selection as text.
    fn paste(&mut self) -> String {
        let (offer, types) = self.state.selection.clone().expect("a selection");
        assert!(types.iter().any(|t| t == TEXT), "offered as text: {types:?}");
        let (read, write) = std::io::pipe().unwrap();
        offer.receive(TEXT.into(), write.as_fd());
        drop(write);
        self.queue.flush().unwrap();
        let reader = std::thread::spawn(move || {
            let mut text = String::new();
            let mut read = read;
            let _ = read.read_to_string(&mut text);
            text
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !reader.is_finished() {
            assert!(Instant::now() < deadline, "the paste never ended");
            self.pump(20);
        }
        reader.join().unwrap()
    }
}

fn shm_buffer(shm: &wl_shm::WlShm, qh: &QueueHandle<Client>, w: i32, h: i32) -> wl_buffer::WlBuffer {
    let bytes: Vec<u8> = std::iter::repeat_n(0xFFFF_FFFFu32.to_le_bytes(), (w * h) as usize).flatten().collect();
    // SAFETY: memfd_create returns a new fd we own (or -1).
    let raw = unsafe { libc::memfd_create(c"clipboard-test".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(raw >= 0);
    // SAFETY: just created.
    let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
    std::fs::File::from(fd.try_clone().unwrap()).write_all(&bytes).unwrap();
    let pool = shm.create_pool(fd.as_fd(), bytes.len() as i32, qh, ());
    let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    buffer
}

#[test]
fn launcher_and_client_share_the_clipboard() {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return;
    };
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    let settings = "[org/gnome/desktop/interface]\nenable-hot-corners=false\n";
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: Some(&shell), size: SIZE, settings, env: Vec::new() }) else { return };
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());

    // The client: a window, its seat's clipboard and primary selection.
    let stream = std::os::unix::net::UnixStream::connect(&session.wayland_display).unwrap();
    let conn = Connection::from_socket(stream).unwrap();
    let (globals, queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=5, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=5, ()).unwrap();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
    let devices: wl_data_device_manager::WlDataDeviceManager = globals.bind(&qh, 1..=3, ()).unwrap();
    let primaries: psm::ZwpPrimarySelectionDeviceManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let _pointer = seat.get_pointer(&qh, ());
    let _keyboard = seat.get_keyboard(&qh, ());
    let device = devices.get_data_device(&seat, &qh, ());
    let primary = primaries.get_device(&seat, &qh, ());
    let surface = compositor.create_surface(&qh, ());
    let xdg = base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_app_id("org.example.ClipboardTest".into());
    toplevel.set_title("Clipboard test".into());
    surface.commit();
    let mut app = App { conn, queue, state: Client::default() };
    app.until("the first configure", |c| c.configure.is_some());
    xdg.ack_configure(app.state.configure.take().unwrap());
    // Big enough to be under the middle of the screen wherever it opens.
    let (w, h) = (900, 540);
    surface.attach(Some(&shm_buffer(&shm, &qh, w, h)), 0, 0);
    surface.frame(&qh, ());
    surface.commit();
    app.until("the window is shown", |c| c.shown);
    app.until("the window has the keyboard", |c| c.focused);

    // A click on the window gives the client a serial to copy with.
    let window_point = (SIZE.0 / 2, SIZE.1 / 2);
    session.input(&format!("click {} {}", window_point.0, window_point.1));
    app.until("a button serial", |c| c.serial.is_some());
    let serial = app.state.serial.unwrap();
    app.state.clipboard_text = "from the client".into();
    app.state.primary_text = "primary words".into();
    let source = devices.create_data_source(&qh, ());
    source.offer(TEXT.into());
    device.set_selection(Some(&source), serial);
    let primary_source = primaries.create_source(&qh, ());
    primary_source.offer(TEXT.into());
    primary.set_selection(Some(&primary_source), serial);
    app.pump_for(200);

    // Ctrl+V in the launcher's search, then a letter; Ctrl+C copies the
    // field; Escape closes the launcher. The client's copy is replaced
    // by the shell's, which the client reads back.
    let open_launcher = |session: &HeadlessSession| {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let conn = session.bus().connect().await;
            conn.call_method(Some("org.gnome.Shell"), "/org/gnome/Shell", Some("org.gnome.Shell"), "ShowApplications", &()).await.expect("ShowApplications");
        });
    };
    open_launcher(&session);
    app.until("the launcher takes the keyboard", |c| !c.focused);
    session.input("tap 29 47");
    app.pump_for(500);
    session.input("type x");
    session.input("tap 29 46");
    app.until("the shell took the clipboard", |c| c.cancelled.contains(&"clipboard"));
    session.input("tap 1");
    app.until("the window has the keyboard again", |c| c.focused);
    app.until("the shell's selection offered", |c| c.selection.as_ref().is_some_and(|(_, t)| t.iter().any(|t| t == TEXT)));
    assert_eq!(app.paste(), "from the clientx", "the launcher pasted the client's text, then copied its field");

    // Middle click on the launcher: the primary selection goes in.
    open_launcher(&session);
    app.until("the launcher takes the keyboard", |c| !c.focused);
    session.input(&format!("click {} {} 274", SIZE.0 / 2, SIZE.1 / 3));
    app.pump_for(500);
    session.input("tap 29 46");
    session.input("tap 1");
    app.until("the window has the keyboard again", |c| c.focused);
    app.pump_for(300);
    let pasted = app.paste();
    assert_eq!(pasted, "primary words", "middle click pasted the primary selection into the launcher");
    drop(app.conn);
}
