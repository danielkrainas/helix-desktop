//! A Wayland client for headless focus tests: toplevels with plain
//! buffers, the keyboard and pointer focus it is told about, the serial
//! of the last key or button press it got, and activation tokens.

#![allow(dead_code)]

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use helix_protocols::gtk_shell_client::{gtk_shell1, gtk_surface1};
use helix_test_support::headless::HeadlessSession;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
pub struct State {
    /// The surface with keyboard focus.
    pub keyboard: Option<wl_surface::WlSurface>,
    /// The surface under the pointer.
    pub pointer: Option<wl_surface::WlSurface>,
    /// Pending configure serials, per xdg_surface.
    configured: Vec<(xdg_surface::XdgSurface, u32)>,
    token: Option<String>,
    /// The serial of the last key or button press this client got.
    pub press: Option<u32>,
}

macro_rules! quiet {
    ($($iface:ty),*) => {$(
        impl Dispatch<$iface, ()> for State {
            fn event(_: &mut Self, _: &$iface, _: <$iface as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
quiet!(wl_compositor::WlCompositor, wl_surface::WlSurface, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, wl_seat::WlSeat, xdg_toplevel::XdgToplevel, xdg_activation_v1::XdgActivationV1, gtk_shell1::GtkShell1, gtk_surface1::GtkSurface1);

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
    fn event(state: &mut Self, surface: &xdg_surface::XdgSurface, event: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = event {
            state.configured.push((surface.clone(), serial));
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(state: &mut Self, _: &wl_keyboard::WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_keyboard::Event::Enter { surface, .. } => state.keyboard = Some(surface),
            wl_keyboard::Event::Leave { .. } => state.keyboard = None,
            wl_keyboard::Event::Key { serial, state: wayland_client::WEnum::Value(wl_keyboard::KeyState::Pressed), .. } => state.press = Some(serial),
            _ => {}
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(state: &mut Self, _: &wl_pointer::WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_pointer::Event::Enter { surface, .. } => state.pointer = Some(surface),
            wl_pointer::Event::Leave { .. } => state.pointer = None,
            wl_pointer::Event::Button { serial, state: wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed), .. } => state.press = Some(serial),
            _ => {}
        }
    }
}
impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ()> for State {
    fn event(state: &mut Self, _: &xdg_activation_token_v1::XdgActivationTokenV1, event: xdg_activation_token_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            state.token = Some(token);
        }
    }
}

fn filled(shm: &wl_shm::WlShm, qh: &QueueHandle<State>, (w, h): (i32, i32)) -> wl_buffer::WlBuffer {
    let bytes: Vec<u8> = std::iter::repeat_n(0xFF80_8080u32.to_le_bytes(), (w * h) as usize).flatten().collect();
    // SAFETY: memfd_create returns a new fd we own (or -1).
    let raw = unsafe { libc::memfd_create(c"modal-test".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(raw >= 0);
    // SAFETY: just created.
    let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
    std::io::Write::write_all(&mut std::fs::File::from(fd.try_clone().unwrap()), &bytes).unwrap();
    let pool = shm.create_pool(fd.as_fd(), bytes.len() as i32, qh, ());
    let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    buffer
}

pub struct Client {
    queue: EventQueue<State>,
    pub qh: QueueHandle<State>,
    pub state: State,
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    base: xdg_wm_base::XdgWmBase,
    activation: xdg_activation_v1::XdgActivationV1,
    pub gtk_shell: gtk_shell1::GtkShell1,
    seat: wl_seat::WlSeat,
}

pub struct Toplevel {
    pub surface: wl_surface::WlSurface,
    pub xdg: xdg_surface::XdgSurface,
    pub toplevel: xdg_toplevel::XdgToplevel,
}

impl Client {
    pub fn roundtrip(&mut self) {
        self.queue.roundtrip(&mut self.state).unwrap();
    }

    /// Run until `done` holds (or fail with `what`).
    pub fn until(&mut self, what: &str, done: impl Fn(&State) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.roundtrip();
            if done(&self.state) {
                return;
            }
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A mapped toplevel of `size`; `before_commit` runs before its
    /// initial commit.
    pub fn toplevel<R>(&mut self, size: (i32, i32), before_commit: impl FnOnce(&Self, &Toplevel) -> R) -> (Toplevel, R) {
        let surface = self.compositor.create_surface(&self.qh, ());
        let xdg = self.base.get_xdg_surface(&surface, &self.qh, ());
        let toplevel = xdg.get_toplevel(&self.qh, ());
        let t = Toplevel { surface, xdg, toplevel };
        let made = before_commit(self, &t);
        t.surface.commit();
        let xdg = t.xdg.clone();
        self.until("no first configure", |s| s.configured.iter().any(|(x, _)| x == &xdg));
        let serial = self.state.configured.iter().rev().find(|(x, _)| x == &xdg).unwrap().1;
        self.state.configured.retain(|(x, _)| x != &xdg);
        t.xdg.ack_configure(serial);
        t.surface.attach(Some(&filled(&self.shm, &self.qh, size)), 0, 0);
        t.surface.damage_buffer(0, 0, size.0, size.1);
        t.surface.commit();
        self.roundtrip();
        (t, made)
    }

    /// A token asked for from `from` (none: no surface) with `serial` on
    /// the seat (none: no serial).
    pub fn token(&mut self, from: Option<&wl_surface::WlSurface>, serial: Option<u32>) -> String {
        self.state.token = None;
        let token = self.activation.get_activation_token(&self.qh, ());
        if let Some(surface) = from {
            token.set_surface(surface);
        }
        if let Some(serial) = serial {
            token.set_serial(serial, &self.seat);
        }
        token.commit();
        self.until("no activation token", |s| s.token.is_some());
        token.destroy();
        self.state.token.clone().unwrap()
    }

    /// Activate `surface` with `token`.
    pub fn activate_token(&mut self, token: String, surface: &wl_surface::WlSurface) {
        self.activation.activate(token, surface);
        self.roundtrip();
        self.roundtrip();
    }

    /// Activate `surface` with a token for the last press this client
    /// got, as Chromium does for its own window after a key press.
    pub fn activate(&mut self, surface: &wl_surface::WlSurface) {
        self.roundtrip();
        let press = self.state.press;
        let token = self.token(Some(surface), press);
        self.activate_token(token, surface);
    }

    pub fn focused(&mut self) -> Option<wl_surface::WlSurface> {
        self.roundtrip();
        self.state.keyboard.clone()
    }
}


/// A client of `session`'s compositor with a keyboard and a pointer.
pub fn connect(session: &HeadlessSession) -> Client {
    let conn = Connection::from_socket(UnixStream::connect(&session.wayland_display).unwrap()).unwrap();
    let (globals, queue) = registry_queue_init::<State>(&conn).unwrap();
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 5..=9, ()).unwrap();
    seat.get_keyboard(&qh, ());
    seat.get_pointer(&qh, ());
    Client {
        compositor: globals.bind(&qh, 4..=6, ()).unwrap(),
        shm: globals.bind(&qh, 1..=1, ()).unwrap(),
        base: globals.bind(&qh, 1..=6, ()).unwrap(),
        activation: globals.bind(&qh, 1..=1, ()).unwrap(),
        gtk_shell: globals.bind(&qh, 1..=4, ()).expect("helix-comp offers gtk_shell1"),
        seat,
        queue,
        qh,
        state: State::default(),
    }
}

/// A point of the screen (`size`) where the pointer is over `surface` of
/// `c`: where windows go is Helix's choice, so look for it.
pub fn point_over(session: &mut HeadlessSession, c: &mut Client, surface: &wl_surface::WlSurface, (w, h): (i32, i32)) -> (i32, i32) {
    let over = (0..w).step_by(40).flat_map(|x| (0..h).step_by(40).map(move |y| (x, y))).find(|(x, y)| {
        session.input(&format!("move {x} {y}"));
        c.roundtrip();
        c.state.pointer.as_ref() == Some(surface)
    });
    over.expect("the pointer is never over the surface")
}
