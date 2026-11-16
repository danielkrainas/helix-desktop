//! Window pictures never pile up (helix-desktop#105): a headless Helix
//! session (helix-comp with helix-shell, scratch home, private bus) with
//! eight client windows (this test). The headless compositor never answers
//! the shell's window-picture requests, so every request the shell does
//! not give up on keeps its memory file (`memfd:helix-capture`) open.
//!
//! - With the overview held open, the pictures asked for are given up on
//!   after their timeout and asked for again: never more than one
//!   request (memory file) per window, and new files replace the old.
//! - Opening and closing the overview 200 times leaves the shell's open
//!   file descriptors (`/proc/<pid>/fd`) where they were.
//!
//! Needs helix-comp built next to this test; skipped otherwise.

use std::collections::HashSet;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};
use smithay_client_toolkit::reexports::client::globals::{registry_queue_init, GlobalListContents};
use smithay_client_toolkit::reexports::client::protocol::{wl_buffer, wl_callback, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, EventQueue, QueueHandle};
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const SIZE: (u32, u32) = (1280, 720);
const WINDOWS: usize = 8;
const ROUNDS: usize = 200;

#[derive(Default)]
struct Client {
    /// Configures to acknowledge, per window.
    configure: Vec<Option<u32>>,
    /// Configures seen.
    configured: usize,
    /// Frames shown.
    shown: usize,
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
quiet!(wl_compositor::WlCompositor, wl_surface::WlSurface, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, xdg_toplevel::XdgToplevel);

impl Dispatch<wl_callback::WlCallback, ()> for Client {
    fn event(state: &mut Self, _: &wl_callback::WlCallback, event: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = event {
            state.shown += 1;
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

impl Dispatch<xdg_surface::XdgSurface, usize> for Client {
    fn event(state: &mut Self, _: &xdg_surface::XdgSurface, event: xdg_surface::Event, index: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = event {
            state.configure[*index] = Some(serial);
            state.configured += 1;
        }
    }
}

struct App {
    queue: EventQueue<Client>,
    state: Client,
    xdg: Vec<xdg_surface::XdgSurface>,
}

impl App {
    /// Dispatch what comes within `ms` (acknowledging configures).
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
        for (xdg, serial) in self.xdg.iter().zip(self.state.configure.iter_mut()) {
            if let Some(serial) = serial.take() {
                xdg.ack_configure(serial);
            }
        }
        self.queue.flush().unwrap();
    }

    fn pump_for(&mut self, ms: u64) {
        let until = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < until {
            self.pump(10);
        }
    }

    fn until(&mut self, what: &str, done: impl Fn(&Client) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(&self.state) {
            assert!(Instant::now() < deadline, "timed out: {what}");
            self.pump(50);
        }
    }
}

fn shm_buffer(shm: &wl_shm::WlShm, qh: &QueueHandle<Client>, w: i32, h: i32, color: u32) -> wl_buffer::WlBuffer {
    let bytes: Vec<u8> = std::iter::repeat_n(color.to_le_bytes(), (w * h) as usize).flatten().collect();
    // SAFETY: memfd_create returns a new fd we own (or -1).
    let raw = unsafe { libc::memfd_create(c"capture-test".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(raw >= 0);
    // SAFETY: just created.
    let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
    std::fs::File::from(fd.try_clone().unwrap()).write_all(&bytes).unwrap();
    let pool = shm.create_pool(fd.as_fd(), bytes.len() as i32, qh, ());
    let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    buffer
}

/// Open or close the overview (`OverviewActive`).
fn overview(session: &HeadlessSession, active: bool) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let conn = session.bus().connect().await;
        let value = zbus::zvariant::Value::from(active);
        conn.call_method(Some("org.gnome.Shell"), "/org/gnome/Shell", Some("org.freedesktop.DBus.Properties"), "Set", &("org.gnome.Shell", "OverviewActive", value))
            .await
            .expect("OverviewActive");
    });
}

/// The session's helix-shell process (the one whose HOME is the
/// session's scratch home).
fn shell_pid(home: &Path) -> Option<u32> {
    let want = format!("HOME={}", home.display());
    std::fs::read_dir("/proc").ok()?.flatten().find_map(|entry| {
        let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
        let comm = std::fs::read_to_string(entry.path().join("comm")).ok()?;
        if comm.trim() != "helix-shell" {
            return None;
        }
        let environ = std::fs::read(entry.path().join("environ")).ok()?;
        environ.split(|b| *b == 0).any(|v| v == want.as_bytes()).then_some(pid)
    })
}

/// How many file descriptors the process has open.
fn open_fds(pid: u32) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/fd")).map(|d| d.count()).unwrap_or(0)
}

/// The window-picture memory files the process has open (their inodes).
fn capture_files(pid: u32) -> HashSet<u64> {
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { return HashSet::new() };
    fds.flatten()
        .filter(|fd| std::fs::read_link(fd.path()).is_ok_and(|l| l.to_string_lossy().contains("memfd:helix-capture")))
        .filter_map(|fd| std::fs::metadata(fd.path()).ok())
        .map(|m| m.ino())
        .collect()
}

/// How many times the compositor saw a `namespace` layer surface made.
fn count_opened(log: &str, namespace: &str) -> usize {
    log.matches(&format!("namespace=\"{namespace}\"")).count()
}

#[test]
fn window_pictures_do_not_pile_up() {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return;
    };
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    let settings = "[org/gnome/desktop/interface]\nenable-hot-corners=false\nenable-animations=false\n";
    let Some(session) = HeadlessSession::start(Options { comp: &comp, shell: Some(&shell), size: SIZE, settings, env: Vec::new() }) else { return };
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());
    let deadline = Instant::now() + Duration::from_secs(10);
    let pid = loop {
        if let Some(pid) = shell_pid(session.home()) {
            break pid;
        }
        assert!(Instant::now() < deadline, "no helix-shell process for the session");
        std::thread::sleep(Duration::from_millis(50));
    };

    // Eight windows.
    let stream = std::os::unix::net::UnixStream::connect(&session.wayland_display).unwrap();
    let conn = Connection::from_socket(stream).unwrap();
    let (globals, queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=5, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=5, ()).unwrap();
    let mut surfaces = Vec::new();
    let mut toplevels = Vec::new();
    let mut xdgs = Vec::new();
    for i in 0..WINDOWS {
        let surface = compositor.create_surface(&qh, ());
        let xdg = base.get_xdg_surface(&surface, &qh, i);
        let toplevel = xdg.get_toplevel(&qh, ());
        toplevel.set_app_id(format!("org.example.CaptureTest{i}"));
        toplevel.set_title(format!("Capture test {i}"));
        surface.commit();
        surfaces.push(surface);
        toplevels.push(toplevel);
        xdgs.push(xdg);
    }
    let mut app = App { queue, state: Client { configure: vec![None; WINDOWS], configured: 0, shown: 0 }, xdg: xdgs };
    app.until("the first configures", |c| c.configured >= WINDOWS);
    for (i, surface) in surfaces.iter().enumerate() {
        surface.attach(Some(&shm_buffer(&shm, &qh, 400, 300, 0xFF20_4060 + i as u32 * 0x10)), 0, 0);
        surface.frame(&qh, ());
        surface.commit();
    }
    app.until("the windows are shown", |c| c.shown >= WINDOWS);
    app.pump_for(1000);

    let wait_overview = |app: &mut App, want: usize| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while count_opened(&session.log(), "helix-overview") < want {
            assert!(Instant::now() < deadline, "the overview did not open:\n{}", session.log());
            app.pump(5);
        }
    };

    // Held open: requests time out and are made again, one per window.
    let before = open_fds(pid);
    overview(&session, true);
    wait_overview(&mut app, 1);
    app.pump_for(500);
    let first = capture_files(pid);
    let mut most = first.len();
    let until = Instant::now() + Duration::from_millis(4500);
    while Instant::now() < until {
        app.pump(100);
        most = most.max(capture_files(pid).len());
    }
    let later = capture_files(pid);
    overview(&session, false);
    app.pump_for(500);
    let after_hold = capture_files(pid).len();
    eprintln!("held open: {} picture files at first, {} after 5 s (most {most}), {} once closed", first.len(), later.len(), after_hold);
    assert!(!first.is_empty(), "the overview asked for window pictures");
    assert!(most <= WINDOWS, "{most} picture requests for {WINDOWS} windows");
    assert!(first.is_disjoint(&later), "the unanswered requests were given up on and made again");
    assert_eq!(after_hold, 0, "closing the overview gave up on its pictures");

    // Opened and closed 200 times.
    let mut most_fds = 0;
    let mut most_files = 0;
    for round in 0..ROUNDS {
        overview(&session, true);
        wait_overview(&mut app, round + 2);
        app.pump_for(20);
        most_files = most_files.max(capture_files(pid).len());
        most_fds = most_fds.max(open_fds(pid));
        overview(&session, false);
        app.pump_for(20);
    }
    app.pump_for(1000);
    let after = open_fds(pid);
    let files = capture_files(pid).len();
    eprintln!("open fds: {before} before, {after} after {ROUNDS} overviews (most {most_fds}); picture files most {most_files}, at the end {files}");
    assert!(most_files <= WINDOWS, "{most_files} picture requests for {WINDOWS} windows");
    assert_eq!(files, 0, "picture files left open");
    assert!(after <= before + 8, "open fds grew from {before} to {after}");
    drop(toplevels);
}
