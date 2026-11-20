//! `helix-agent clipboard` end to end (helixdesktop/spec
//! `contracts/agent-access/cli.md`; helixdesktop/desktop#151): a headless
//! Helix session (helix-comp with helix-shell, scratch home, private bus)
//! and a Wayland client window (this test) sharing the real clipboard.
//!
//! - The client copies; `clipboard get` and `clipboard types` read it.
//! - A password manager's copy (`x-kde-passwordManagerHint: secret`) and
//!   an excluded app's copy are refused (exit 14).
//! - `clipboard set` puts text, and a picture from standard input, on the
//!   clipboard; the client pastes them, and `get --output` writes the
//!   picture to a file.
//! - The activity log has none of it.
//!
//! Needs helix-comp and helix-shell built next to this test; skipped
//! otherwise.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};
use serde_json::Value as Json;
use smithay_client_toolkit::reexports::client::globals::{registry_queue_init, GlobalListContents};
use smithay_client_toolkit::reexports::client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use smithay_client_toolkit::reexports::client::{event_created_child, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const CLI: &str = env!("CARGO_BIN_EXE_helix-agent");
const TEXT: &str = "text/plain;charset=utf-8";
const HINT: &str = "x-kde-passwordManagerHint";
const APP_ID: &str = "org.example.ClipboardTest";
const SIZE: (u32, u32) = (1024, 640);
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR not really a picture";

#[derive(Default)]
struct OfferData(Mutex<Vec<String>>);

#[derive(Default)]
struct Client {
    configure: Option<u32>,
    shown: bool,
    focused: bool,
    serial: Option<u32>,
    selection: Option<(wl_data_offer::WlDataOffer, Vec<String>)>,
    /// What our source serves, by type.
    serving: HashMap<String, Vec<u8>>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Client {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

macro_rules! quiet {
    ($($iface:ty),*) => {$(
        impl Dispatch<$iface, ()> for Client {
            fn event(_: &mut Self, _: &$iface, _: <$iface as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
quiet!(wl_compositor::WlCompositor, wl_surface::WlSurface, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, wl_seat::WlSeat, wl_data_device_manager::WlDataDeviceManager, xdg_toplevel::XdgToplevel);

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

impl Dispatch<wl_data_source::WlDataSource, ()> for Client {
    fn event(state: &mut Self, _: &wl_data_source::WlDataSource, event: wl_data_source::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_data_source::Event::Send { mime_type, fd } = event {
            let data = state.serving.get(&mime_type).cloned().unwrap_or_default();
            let mut file = std::fs::File::from(fd);
            let _ = file.write_all(&data);
        }
    }
}

struct App {
    queue: EventQueue<Client>,
    state: Client,
    devices: wl_data_device_manager::WlDataDeviceManager,
    device: wl_data_device::WlDataDevice,
    qh: QueueHandle<Client>,
}

impl App {
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

    fn until(&mut self, what: &str, done: impl Fn(&Client) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(&self.state) {
            assert!(Instant::now() < deadline, "timed out: {what}");
            self.pump(50);
        }
    }

    /// Copy `content` (type, data) with the last input serial.
    fn copy(&mut self, content: &[(&str, &[u8])]) {
        self.state.serving = content.iter().map(|(t, d)| ((*t).to_owned(), d.to_vec())).collect();
        let source = self.devices.create_data_source(&self.qh, ());
        for (t, _) in content {
            source.offer((*t).to_owned());
        }
        self.device.set_selection(Some(&source), self.state.serial.expect("a serial"));
        let first = content[0].0.to_owned();
        self.until("our own selection offered", |c| c.selection.as_ref().is_some_and(|(_, t)| t.contains(&first)));
    }

    /// Paste the clipboard as `mime`, while the CLI or the shell serves it.
    fn paste(&mut self, mime: &str) -> Vec<u8> {
        let (offer, types) = self.state.selection.clone().expect("a selection");
        assert!(types.iter().any(|t| t == mime), "offered as {mime}: {types:?}");
        let (read, write) = std::io::pipe().unwrap();
        offer.receive(mime.into(), write.as_fd());
        drop(write);
        self.queue.flush().unwrap();
        let reader = std::thread::spawn(move || {
            let mut data = Vec::new();
            let mut read = read;
            let _ = read.read_to_end(&mut data);
            data
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

/// Run the CLI on `bus` with `home` as every XDG directory, `input` on
/// its standard input. The client keeps being dispatched meanwhile (it
/// serves the clipboard).
fn run(app: &mut App, bus: &str, home: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(CLI)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("LANG", "C.UTF-8")
        .env("DBUS_SESSION_BUS_ADDRESS", bus)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run helix-agent");
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let waiter = std::thread::spawn(move || child.wait_with_output().unwrap());
    let deadline = Instant::now() + Duration::from_secs(30);
    while !waiter.is_finished() {
        assert!(Instant::now() < deadline, "helix-agent {args:?} did not end");
        app.pump(20);
    }
    waiter.join().unwrap()
}

fn json(out: &Output) -> Json {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)))
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("exited")
}

#[test]
fn clipboard_from_the_command_line() {
    let (Some(comp), Some(shell)) = (sibling_binary("helix-comp"), sibling_binary("helix-shell")) else {
        eprintln!("helix-comp or helix-shell not built; skipping");
        return;
    };
    let schemas = std::env::temp_dir().join(format!("helix-agent-clipboard-schemas-{}", std::process::id()));
    std::fs::create_dir_all(&schemas).unwrap();
    for schema in ["org.helixdesktop.agent-access", "org.helixdesktop.shell", "org.helixdesktop.extensions"] {
        let data = format!("{}/../../data/{schema}.gschema.xml", env!("CARGO_MANIFEST_DIR"));
        std::fs::copy(data, schemas.join(format!("{schema}.gschema.xml"))).unwrap();
    }
    let Ok(compiled) = Command::new("glib-compile-schemas").arg(&schemas).status() else {
        eprintln!("glib-compile-schemas is not installed; skipping");
        return;
    };
    assert!(compiled.success());
    let on = "[org/gnome/desktop/interface]\nenable-hot-corners=false\n\n[org/helixdesktop/agent-access]\nenabled=true\nallow-remote=true\naccess-level='desktop'\nremember-mode='remember'\nclipboard=true\n";
    // This test runs the CLI, so the shell sees this test as the agent;
    // the user allowed it to control the desktop.
    let me = std::env::current_exe().unwrap().to_string_lossy().into_owned();
    let grants = format!("version = 1\n\n[[agent]]\nid = \"{me}\"\nexecutable = \"{me}\"\nname = \"\"\nlevel = \"desktop\"\ngranted = 1\n");
    let Some(mut session) = HeadlessSession::start_prepared(
        Options { comp: &comp, shell: Some(&shell), size: SIZE, settings: on, env: vec![("GSETTINGS_SCHEMA_DIR".into(), schemas.to_string_lossy().into_owned())] },
        |home| {
            std::fs::create_dir_all(home.join(".config/helix")).unwrap();
            std::fs::write(home.join(".config/helix/agent-grants.toml"), &grants).unwrap();
        },
    ) else {
        return;
    };
    assert!(session.wait_for_log("shell ready", Duration::from_secs(30)), "the shell did not start:\n{}", session.log());
    let (bus, home) = (session.bus().address().to_owned(), session.home().to_owned());
    let keyfile = home.join(".config/glib-2.0/settings/keyfile");

    // The client: a window and its seat's clipboard.
    let stream = std::os::unix::net::UnixStream::connect(&session.wayland_display).unwrap();
    let conn = Connection::from_socket(stream).unwrap();
    let (globals, queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=5, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=5, ()).unwrap();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
    let devices: wl_data_device_manager::WlDataDeviceManager = globals.bind(&qh, 1..=3, ()).unwrap();
    let _pointer = seat.get_pointer(&qh, ());
    let _keyboard = seat.get_keyboard(&qh, ());
    let device = devices.get_data_device(&seat, &qh, ());
    let surface = compositor.create_surface(&qh, ());
    let xdg = base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_app_id(APP_ID.into());
    toplevel.set_title("Clipboard test".into());
    surface.commit();
    let mut app = App { queue, state: Client::default(), devices, device, qh: qh.clone() };
    app.until("the first configure", |c| c.configure.is_some());
    xdg.ack_configure(app.state.configure.take().unwrap());
    let (w, h) = (900, 540);
    surface.attach(Some(&shm_buffer(&shm, &qh, w, h)), 0, 0);
    surface.frame(&qh, ());
    surface.commit();
    app.until("the window is shown", |c| c.shown);
    app.until("the window has the keyboard", |c| c.focused);
    session.input(&format!("click {} {}", SIZE.0 / 2, SIZE.1 / 2));
    app.until("a button serial", |c| c.serial.is_some());

    // The client copies; the CLI reads it, as text and as JSON.
    app.copy(&[(TEXT, b"Dear diary"), ("text/html", b"<p>Dear diary</p>")]);
    let out = run(&mut app, &bus, &home, &["--text", "clipboard", "get"], b"");
    assert_eq!((code(&out), out.stdout.as_slice()), (0, b"Dear diary".as_slice()), "{}", String::from_utf8_lossy(&out.stderr));
    let out = run(&mut app, &bus, &home, &["clipboard", "get", "--type", "text/html"], b"");
    assert_eq!(json(&out)["text"], "<p>Dear diary</p>");
    assert_eq!(json(&out)["mime-type"], "text/html");
    let types = json(&run(&mut app, &bus, &home, &["clipboard", "types"], b""));
    assert_eq!(types["types"], serde_json::json!([TEXT, "text/html"]));

    // A password manager's copy: refused.
    app.copy(&[(TEXT, b"hunter2"), (HINT, b"secret")]);
    let out = run(&mut app, &bus, &home, &["clipboard", "get"], b"");
    assert_eq!(code(&out), 14, "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(json(&out)["error"]["message"], "Agents can't use password fields");
    // An excluded app's copy: refused.
    std::fs::write(&keyfile, format!("{on}excluded-apps=['{APP_ID}']\n")).unwrap();
    app.copy(&[(TEXT, b"private words")]);
    let deadline = Instant::now() + Duration::from_secs(10);
    let out = loop {
        let out = run(&mut app, &bus, &home, &["clipboard", "get"], b"");
        if code(&out) != 0 || Instant::now() > deadline {
            break out;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!((code(&out), json(&out)["error"]["code"].clone()), (14, Json::from("excluded")), "{}", String::from_utf8_lossy(&out.stdout));
    std::fs::write(&keyfile, on).unwrap();

    // The CLI sets text; the client pastes it.
    let out = run(&mut app, &bus, &home, &["clipboard", "set", "From the agent"], b"");
    assert_eq!((code(&out), json(&out)), (0, serde_json::json!({})), "{}", String::from_utf8_lossy(&out.stdout));
    app.until("the agent's text offered", |c| c.selection.as_ref().is_some_and(|(_, t)| t.iter().any(|t| t == "UTF8_STRING")));
    assert_eq!(app.paste(TEXT), b"From the agent");
    let out = run(&mut app, &bus, &home, &["--text", "clipboard", "get"], b"");
    assert_eq!(out.stdout, b"From the agent", "an agent's copy reads back");
    // A picture from standard input; read back into a file and through a
    // pipe.
    let out = run(&mut app, &bus, &home, &["clipboard", "set", "--type", "image/png"], PNG);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stdout));
    app.until("the picture offered", |c| c.selection.as_ref().is_some_and(|(_, t)| t == &["image/png"]));
    assert_eq!(app.paste("image/png"), PNG);
    let file = home.join("picture.png");
    let out = run(&mut app, &bus, &home, &["clipboard", "get", "--type", "image/*", "--output", file.to_str().unwrap()], b"");
    assert_eq!(json(&out)["path"], file.to_str().unwrap(), "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(std::fs::read(&file).unwrap(), PNG);
    let out = run(&mut app, &bus, &home, &["--text", "clipboard", "get"], b"");
    assert_eq!(out.stdout, PNG, "binary to a pipe");
    // Types an agent may not set.
    assert_eq!(code(&run(&mut app, &bus, &home, &["clipboard", "set", "--type", "image/gif", "x"], b"")), 2);

    // The activity log: kinds and lengths only.
    let dir = home.join(".local/state/helix/agent-activity");
    let all: String = std::fs::read_dir(&dir).unwrap().flatten().map(|e| std::fs::read_to_string(e.path()).unwrap()).collect();
    assert!(all.contains("\"method\":\"GetClipboard\"") && all.contains("\"method\":\"SetClipboard\""), "{all}");
    assert!(all.contains(&format!("\"target\":\"{APP_ID}\"")), "{all}");
    for secret in ["diary", "hunter2", "private words", "From the agent", "really a picture"] {
        assert!(!all.contains(secret), "{secret} is in the log");
    }
    let log = session.log();
    for secret in ["diary", "hunter2", "private words", "From the agent"] {
        assert!(!log.contains(secret), "{secret} is in the session's log");
    }
    drop(conn);
    let _ = std::fs::remove_dir_all(&schemas);
}
