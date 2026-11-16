//! Focus meant for a window goes to its modal dialog (helixdesktop/spec
//! `contracts/wayland/README.md`, "Window-management settings", Focus),
//! with the dialog made modal the way GTK 3 does it
//! (`gtk_surface1.set_modal`). This is what Electron apps such as VS Code
//! do with a "Save changes?" dialog: they activate their main window
//! (`xdg_activation_v1`, with a fresh token) right after opening the
//! dialog, and give the dialog its parent only after that. Before the
//! fix the main window kept focus and the dialog took no keys.
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use helix_protocols::gtk_shell_client::{gtk_shell1, gtk_surface1};
use helix_test_support::headless::{HeadlessSession, Options};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
struct State {
    /// The surface with keyboard focus.
    keyboard: Option<wl_surface::WlSurface>,
    /// The surface under the pointer.
    pointer: Option<wl_surface::WlSurface>,
    /// Pending configure serials, per xdg_surface.
    configured: Vec<(xdg_surface::XdgSurface, u32)>,
    token: Option<String>,
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
            _ => {}
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(state: &mut Self, _: &wl_pointer::WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_pointer::Event::Enter { surface, .. } => state.pointer = Some(surface),
            wl_pointer::Event::Leave { .. } => state.pointer = None,
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

struct Client {
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    base: xdg_wm_base::XdgWmBase,
    activation: xdg_activation_v1::XdgActivationV1,
    gtk_shell: gtk_shell1::GtkShell1,
}

struct Toplevel {
    surface: wl_surface::WlSurface,
    xdg: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
}

impl Client {
    fn roundtrip(&mut self) {
        self.queue.roundtrip(&mut self.state).unwrap();
    }

    /// Run until `done` holds (or fail with `what`).
    fn until(&mut self, what: &str, done: impl Fn(&State) -> bool) {
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
    fn toplevel<R>(&mut self, size: (i32, i32), before_commit: impl FnOnce(&Self, &Toplevel) -> R) -> (Toplevel, R) {
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

    /// Activate `surface` with a fresh token, as Chromium does for its
    /// own window.
    fn activate(&mut self, surface: &wl_surface::WlSurface) {
        self.state.token = None;
        let token = self.activation.get_activation_token(&self.qh, ());
        token.set_surface(surface);
        token.commit();
        self.until("no activation token", |s| s.token.is_some());
        token.destroy();
        self.activation.activate(self.state.token.clone().unwrap(), surface);
        self.roundtrip();
        self.roundtrip();
    }

    fn focused(&mut self) -> Option<wl_surface::WlSurface> {
        self.roundtrip();
        self.state.keyboard.clone()
    }
}

#[test]
fn focus_goes_to_the_modal_dialog() {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: None, size: (1280, 800), settings: "", env: Vec::new() }) else { return };
    let conn = Connection::from_socket(UnixStream::connect(&session.wayland_display).unwrap()).unwrap();
    let (globals, queue) = registry_queue_init::<State>(&conn).unwrap();
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 5..=9, ()).unwrap();
    seat.get_keyboard(&qh, ());
    seat.get_pointer(&qh, ());
    let mut c = Client {
        compositor: globals.bind(&qh, 4..=6, ()).unwrap(),
        shm: globals.bind(&qh, 1..=1, ()).unwrap(),
        base: globals.bind(&qh, 1..=6, ()).unwrap(),
        activation: globals.bind(&qh, 1..=1, ()).unwrap(),
        gtk_shell: globals.bind(&qh, 1..=4, ()).expect("helix-comp offers gtk_shell1"),
        queue,
        qh,
        state: State::default(),
    };
    let (main, ()) = c.toplevel((800, 600), |_, _| {});
    let main_surface = main.surface.clone();
    c.until("the new main window has no focus", |s| s.keyboard.as_ref() == Some(&main_surface));

    // A modal dialog of it, made modal as GTK 3 does.
    let (dialog, gtk) = c.toplevel((200, 100), |c, t| {
        let gtk = c.gtk_shell.get_gtk_surface(&t.surface, &c.qh, ());
        gtk.set_modal();
        t.toplevel.set_parent(Some(&main.toplevel));
        gtk
    });
    let dialog_surface = dialog.surface.clone();
    c.until("the new dialog has no focus", |s| s.keyboard.as_ref() == Some(&dialog_surface));

    // The app activates its main window: the dialog keeps focus.
    c.activate(&main.surface);
    assert_eq!(c.focused().as_ref(), Some(&dialog.surface), "activating the main window took focus from its modal dialog");

    // A click on the main window (outside the dialog): still the dialog.
    // Where it is is Helix's choice: look for it.
    let over_main = (0..1280).step_by(40).flat_map(|x| (0..800).step_by(40).map(move |y| (x, y))).find(|(x, y)| {
        session.input(&format!("move {x} {y}"));
        c.roundtrip();
        c.state.pointer.as_ref() == Some(&main_surface)
    });
    let (x, y) = over_main.expect("the pointer is never over the main window");
    session.input(&format!("click {x} {y}"));
    assert_eq!(c.focused().as_ref(), Some(&dialog.surface), "a click on the main window took focus from its modal dialog");

    // The parent comes after the activation (VS Code's order): when the
    // dialog gets its parent, focus moves on to it.
    dialog.toplevel.set_parent(None);
    c.roundtrip();
    c.activate(&main.surface);
    assert_eq!(c.focused().as_ref(), Some(&main.surface), "an activation with a fresh token was not granted");
    dialog.toplevel.set_parent(Some(&main.toplevel));
    c.until("the dialog did not get focus when it got its parent", |s| s.keyboard.as_ref() == Some(&dialog_surface));

    // A dialog that is not modal does not keep focus from its parent.
    gtk.unset_modal();
    c.roundtrip();
    c.activate(&main.surface);
    assert_eq!(c.focused().as_ref(), Some(&main.surface), "a dialog that is not modal kept focus from its parent");
    // Made modal again while its parent has focus: focus moves to it.
    gtk.set_modal();
    c.until("the dialog did not get focus when made modal", |s| s.keyboard.as_ref() == Some(&dialog_surface));

    let log = session.log();
    assert!(!log.contains("panicked"), "{log}");
}
