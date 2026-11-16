//! The headless backend ignores tearing hints safely (helix-desktop-spec
//! `contracts/wayland/README.md`, "Tearing"): the real `helix-comp`
//! headless offers `wp_tearing_control_v1`; a full-screen client asking
//! for `async` keeps being shown frame after frame (the snapshot follows
//! its colour), its frame callbacks keep coming, and nothing tries an
//! asynchronous page flip. Needs `dbus-daemon` and Xwayland (as every
//! headless session); skipped otherwise.

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use helix_test_support::headless::{HeadlessSession, Options};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_callback, wl_compositor, wl_shm};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::tearing_control::v1::client::wp_tearing_control_manager_v1::WpTearingControlManagerV1;
use wayland_protocols::wp::tearing_control::v1::client::wp_tearing_control_v1::PresentationHint;
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const SIZE: (i32, i32) = (640, 400);

#[derive(Default)]
struct State {
    size: (i32, i32),
    fullscreen: bool,
    serial: Option<u32>,
    frames_done: u32,
}

macro_rules! quiet {
    ($($iface:ty),*) => {$(
        impl Dispatch<$iface, ()> for State {
            fn event(_: &mut Self, _: &$iface, _: <$iface as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
quiet!(
    wl_compositor::WlCompositor,
    wayland_client::protocol::wl_surface::WlSurface,
    wl_shm::WlShm,
    wayland_client::protocol::wl_shm_pool::WlShmPool,
    wayland_client::protocol::wl_buffer::WlBuffer,
    WpTearingControlManagerV1,
    wayland_protocols::wp::tearing_control::v1::client::wp_tearing_control_v1::WpTearingControlV1
);

impl Dispatch<wayland_client::protocol::wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for State {
    fn event(_: &mut Self, _: &wayland_client::protocol::wl_registry::WlRegistry, _: wayland_client::protocol::wl_registry::Event, _: &wayland_client::globals::GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
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
            state.fullscreen = states.as_chunks::<4>().0.iter().map(|c| u32::from_ne_bytes(*c)).any(|s| xdg_toplevel::State::try_from(s) == Ok(xdg_toplevel::State::Fullscreen));
        }
    }
}
impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(state: &mut Self, _: &wl_callback::WlCallback, event: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = event {
            state.frames_done += 1;
        }
    }
}

fn filled(shm: &wl_shm::WlShm, qh: &QueueHandle<State>, (w, h): (i32, i32), argb: u32) -> wayland_client::protocol::wl_buffer::WlBuffer {
    let bytes: Vec<u8> = std::iter::repeat_n(argb.to_le_bytes(), (w * h) as usize).flatten().collect();
    // SAFETY: memfd_create returns a new fd we own (or -1).
    let raw = unsafe { libc::memfd_create(c"tearing-test".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(raw >= 0);
    // SAFETY: just created.
    let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
    std::io::Write::write_all(&mut std::fs::File::from(fd.try_clone().unwrap()), &bytes).unwrap();
    let pool = shm.create_pool(fd.as_fd(), bytes.len() as i32, qh, ());
    let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    buffer
}

fn center(session: &HeadlessSession) -> u32 {
    let (w, _, data) = session.fresh_snapshot();
    let i = (((SIZE.1 as u32 / 2) * w + SIZE.0 as u32 / 2) * 4) as usize;
    u32::from(data[i]) << 16 | u32::from(data[i + 1]) << 8 | u32::from(data[i + 2])
}

#[test]
fn headless_ignores_the_hint() {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let env = vec![("RUST_LOG".to_owned(), "info".to_owned())];
    let Some(session) = HeadlessSession::start(Options { comp: &comp, shell: None, size: (SIZE.0 as u32, SIZE.1 as u32), settings: "", env }) else { return };
    let conn = Connection::from_socket(UnixStream::connect(&session.wayland_display).unwrap()).unwrap();
    let (globals, mut queue) = registry_queue_init::<State>(&conn).unwrap();
    let qh = queue.handle();
    let mut state = State::default();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ()).unwrap();
    let tearing: WpTearingControlManagerV1 = globals.bind(&qh, 1..=1, ()).expect("helix-comp offers wp_tearing_control_manager_v1");
    let surface = compositor.create_surface(&qh, ());
    let xdg = base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_fullscreen(None);
    let control = tearing.get_tearing_control(&surface, &qh, ());
    control.set_presentation_hint(PresentationHint::Async);
    surface.commit();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !(state.fullscreen && state.serial.is_some()) {
        assert!(Instant::now() < deadline, "not made full screen");
        queue.roundtrip(&mut state).unwrap();
    }
    // Frames of alternating colours, each waiting for its frame callback.
    for (n, color) in [0xFF30_6090u32, 0xFF90_3060, 0xFF30_9060].into_iter().enumerate() {
        if let Some(serial) = state.serial.take() {
            xdg.ack_configure(serial);
        }
        let size = if state.size.0 > 0 { state.size } else { SIZE };
        surface.attach(Some(&filled(&shm, &qh, size, color)), 0, 0);
        surface.damage_buffer(0, 0, size.0, size.1);
        surface.frame(&qh, ());
        surface.commit();
        let deadline = Instant::now() + Duration::from_secs(5);
        while state.frames_done <= n as u32 {
            assert!(Instant::now() < deadline, "frame {n} was not presented");
            queue.roundtrip(&mut state).unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while center(&session) != color & 0xFFFFFF {
            assert!(Instant::now() < deadline, "frame {n} not shown");
        }
    }
    let log = session.log();
    assert!(!log.contains("asynchronous page flips"), "no async flips headless:\n{log}");
    assert!(!log.contains("panicked"), "{log}");
}
