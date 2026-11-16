//! Game overlays over a full-screen game (helixdesktop/spec
//! `contracts/wayland/README.md`, "Layer shell for other clients"): the
//! real `helix-comp` headless (HELIX_HEADLESS_INPUT for input, its
//! snapshot for what is shown), a full-screen client standing in for a
//! game, and a second client, a third-party overlay tool, with a HUD on
//! the overlay layer (no keyboard, an empty input region, an exclusive
//! zone) and a bar on the top layer (an exclusive zone too).
//!
//! - The HUD is drawn above the game; the bar is hidden while the game is
//!   full screen and comes back when it leaves full screen.
//! - Neither exclusive zone shrinks the full-screen game, which stays
//!   full screen.
//! - The HUD does not take the keyboard: keys keep going to the game.
//! - Pointer input goes through the HUD to the game (its input region is
//!   empty), and a click there focuses the game as a click on it would.
//! - The HUD takes the keyboard when it asks for it (`exclusive`) and
//!   gives it back to the game when it stops asking.
//!
//! With `HELIX_GAME_OVERLAY_PNG_DIR` the snapshots are kept there. Needs
//! `dbus-daemon` and Xwayland (as every headless session); skipped
//! otherwise. MangoHud's `mangoapp` is not used: distributions don't ship
//! it (Debian's and Ubuntu's `mangohud` has only the Vulkan/OpenGL layer,
//! which draws into the game's own frames), so a small client stands in.

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use helix_test_support::headless::{HeadlessSession, Options};
use wayland_client::globals::{registry_queue_init, GlobalList};
use wayland_client::protocol::{wl_buffer, wl_compositor, wl_shm, wl_surface};
use wayland_client::{Connection, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

const SIZE: (i32, i32) = (1280, 800);
const GAME: u32 = 0xFF20_6040;
const HUD: u32 = 0xFFE0_2020;
const BAR: u32 = 0xFF20_20E0;
/// The HUD: top-left corner, this size.
const HUD_SIZE: (i32, i32) = (320, 160);
const BAR_H: i32 = 60;

mod client {
    use wayland_client::backend::ObjectId;
    use wayland_client::globals::GlobalListContents;
    use wayland_client::protocol::{wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface};
    use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
    use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
    use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

    #[derive(Default, Debug)]
    pub struct State {
        /// The toplevel's last configure: size, full screen, serial.
        pub size: (i32, i32),
        pub fullscreen: bool,
        pub serial: Option<u32>,
        /// Each layer surface's (by tag) last configure: serial and size.
        pub layer_configures: Vec<(u32, u32, (u32, u32))>,
        /// The surface with this client's keyboard focus, and how many
        /// times it entered; keys received.
        pub keyboard_focus: Option<ObjectId>,
        pub keyboard_enters: u32,
        pub keys: Vec<u32>,
        /// The surface with this client's pointer, and buttons received.
        pub pointer_focus: Option<ObjectId>,
        pub pointer_enters: u32,
        pub buttons: u32,
    }

    macro_rules! quiet {
        ($($iface:ty),*) => {$(
            impl Dispatch<$iface, ()> for State {
                fn event(_: &mut Self, _: &$iface, _: <$iface as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
            }
        )*};
    }
    quiet!(wl_compositor::WlCompositor, wl_surface::WlSurface, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, wl_region::WlRegion, wl_seat::WlSeat, zwlr_layer_shell_v1::ZwlrLayerShellV1);

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
    impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, u32> for State {
        fn event(state: &mut Self, _: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, event: zwlr_layer_surface_v1::Event, tag: &u32, _: &Connection, _: &QueueHandle<Self>) {
            if let zwlr_layer_surface_v1::Event::Configure { serial, width, height } = event {
                state.layer_configures.push((*tag, serial, (width, height)));
            }
        }
    }
    impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
        fn event(state: &mut Self, _: &wl_keyboard::WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            match event {
                wl_keyboard::Event::Enter { surface, .. } => {
                    state.keyboard_focus = Some(surface.id());
                    state.keyboard_enters += 1;
                }
                wl_keyboard::Event::Leave { .. } => state.keyboard_focus = None,
                wl_keyboard::Event::Key { key, state: WEnum::Value(wl_keyboard::KeyState::Pressed), .. } => state.keys.push(key),
                _ => {}
            }
        }
    }
    impl Dispatch<wl_pointer::WlPointer, ()> for State {
        fn event(state: &mut Self, _: &wl_pointer::WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            match event {
                wl_pointer::Event::Enter { surface, .. } => {
                    state.pointer_focus = Some(surface.id());
                    state.pointer_enters += 1;
                }
                wl_pointer::Event::Leave { .. } => state.pointer_focus = None,
                wl_pointer::Event::Button { state: WEnum::Value(wl_pointer::ButtonState::Pressed), .. } => state.buttons += 1,
                _ => {}
            }
        }
    }
}

/// One Wayland client of the session, with a keyboard and a pointer.
struct Client {
    queue: EventQueue<client::State>,
    state: client::State,
    qh: QueueHandle<client::State>,
    globals: GlobalList,
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
}

impl Client {
    fn connect(display: &Path) -> Self {
        let stream = UnixStream::connect(display).expect("connect to the compositor");
        let conn = Connection::from_socket(stream).unwrap();
        let (globals, queue) = registry_queue_init::<client::State>(&conn).unwrap();
        let qh = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
        let seat: wayland_client::protocol::wl_seat::WlSeat = globals.bind(&qh, 5..=7, ()).unwrap();
        seat.get_keyboard(&qh, ());
        seat.get_pointer(&qh, ());
        let mut client = Self { queue, state: client::State::default(), qh, globals, compositor, shm };
        client.roundtrip();
        client
    }

    fn roundtrip(&mut self) {
        self.queue.roundtrip(&mut self.state).unwrap();
    }

    /// A `w`×`h` buffer of one ARGB color.
    fn filled(&self, w: i32, h: i32, argb: u32) -> wl_buffer::WlBuffer {
        let bytes: Vec<u8> = std::iter::repeat_n(argb.to_le_bytes(), (w * h) as usize).flatten().collect();
        // SAFETY: memfd_create returns a new fd we own (or -1).
        let raw = unsafe { libc::memfd_create(c"overlay-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(raw >= 0);
        // SAFETY: just created.
        let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
        std::io::Write::write_all(&mut std::fs::File::from(fd.try_clone().unwrap()), &bytes).unwrap();
        let pool = self.shm.create_pool(fd.as_fd(), bytes.len() as i32, &self.qh, ());
        let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, &self.qh, ());
        pool.destroy();
        buffer
    }
}

/// The game: a full-screen toplevel of one color.
struct Game {
    client: Client,
    surface: wl_surface::WlSurface,
    xdg: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
}

impl Game {
    fn start(display: &Path) -> Self {
        let mut client = Client::connect(display);
        let base: xdg_wm_base::XdgWmBase = client.globals.bind(&client.qh, 1..=6, ()).unwrap();
        let surface = client.compositor.create_surface(&client.qh, ());
        let xdg = base.get_xdg_surface(&surface, &client.qh, ());
        let toplevel = xdg.get_toplevel(&client.qh, ());
        toplevel.set_app_id("org.example.Game".into());
        toplevel.set_title("Game".into());
        toplevel.set_fullscreen(None);
        surface.commit();
        client.roundtrip();
        let mut game = Self { client, surface, xdg, toplevel };
        game.settle();
        game
    }

    /// Answer a pending configure by drawing at its size.
    fn settle(&mut self) {
        self.client.roundtrip();
        if let Some(serial) = self.client.state.serial.take() {
            self.xdg.ack_configure(serial);
            let (w, h) = if self.client.state.size.0 > 0 { self.client.state.size } else { (640, 480) };
            self.xdg.set_window_geometry(0, 0, w, h);
            self.surface.attach(Some(&self.client.filled(w, h, GAME)), 0, 0);
            self.surface.commit();
            self.client.roundtrip();
        }
    }

    fn focused(&self) -> bool {
        self.client.state.keyboard_focus.as_ref() == Some(&self.surface.id())
    }
}

/// The overlay tool: a HUD on the overlay layer and a bar on the top
/// layer.
struct Overlay {
    client: Client,
    hud: (wl_surface::WlSurface, zwlr_layer_surface_v1::ZwlrLayerSurfaceV1),
    bar: (wl_surface::WlSurface, zwlr_layer_surface_v1::ZwlrLayerSurfaceV1),
}

impl Overlay {
    fn start(display: &Path) -> Self {
        use zwlr_layer_surface_v1::Anchor;
        let mut client = Client::connect(display);
        let shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = client.globals.bind(&client.qh, 1..=4, ()).unwrap();
        let make = |client: &Client, tag: u32, layer, namespace: &str, anchor, size: (u32, u32), zone: i32| {
            let surface = client.compositor.create_surface(&client.qh, ());
            let layer = shell.get_layer_surface(&surface, None, layer, namespace.into(), &client.qh, tag);
            layer.set_size(size.0, size.1);
            layer.set_anchor(anchor);
            layer.set_exclusive_zone(zone);
            layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
            surface.commit();
            (surface, layer)
        };
        let hud = make(&client, 1, zwlr_layer_shell_v1::Layer::Overlay, "test-hud", Anchor::Top | Anchor::Left, (HUD_SIZE.0 as u32, HUD_SIZE.1 as u32), HUD_SIZE.1);
        // Click-through: an empty input region.
        let region = client.compositor.create_region(&client.qh, ());
        hud.0.set_input_region(Some(&region));
        region.destroy();
        let bar = make(&client, 2, zwlr_layer_shell_v1::Layer::Top, "test-bar", Anchor::Bottom | Anchor::Left | Anchor::Right, (0, BAR_H as u32), BAR_H);
        client.roundtrip();
        let mut overlay = Self { client, hud, bar };
        overlay.settle();
        overlay
    }

    /// Answer configures by drawing at their sizes.
    fn settle(&mut self) {
        self.client.roundtrip();
        for (tag, serial, (w, h)) in std::mem::take(&mut self.client.state.layer_configures) {
            let ((surface, layer), color, fallback) = if tag == 1 { (&self.hud, HUD, HUD_SIZE) } else { (&self.bar, BAR, (SIZE.0, BAR_H)) };
            layer.ack_configure(serial);
            let (w, h) = (if w > 0 { w as i32 } else { fallback.0 }, if h > 0 { h as i32 } else { fallback.1 });
            surface.attach(Some(&self.client.filled(w, h, color)), 0, 0);
            surface.commit();
        }
        self.client.roundtrip();
    }

    fn hud_keyboard(&mut self, interactivity: zwlr_layer_surface_v1::KeyboardInteractivity) {
        self.hud.1.set_keyboard_interactivity(interactivity);
        self.hud.0.commit();
        self.settle();
    }

    fn hud_focused(&self) -> bool {
        self.client.state.keyboard_focus.as_ref() == Some(&self.hud.0.id())
    }
}

/// The snapshot's RGB at (x, y), as 0xRRGGBB.
fn rgb(image: &(u32, u32, Vec<u8>), x: i32, y: i32) -> u32 {
    let (w, _, data) = image;
    let i = ((y as u32 * w + x as u32) * 4) as usize;
    u32::from(data[i]) << 16 | u32::from(data[i + 1]) << 8 | u32::from(data[i + 2])
}

/// A snapshot taken after everything so far was drawn.
fn snapshot(session: &HeadlessSession, name: &str) -> (u32, u32, Vec<u8>) {
    std::thread::sleep(Duration::from_millis(200));
    let image = session.fresh_snapshot();
    if let Some(dir) = std::env::var_os("HELIX_GAME_OVERLAY_PNG_DIR") {
        let _ = std::fs::copy(session.snapshot_path(), Path::new(&dir).join(format!("{name}.png")));
    }
    image
}

/// Settle both clients until `done` holds.
fn until(game: &mut Game, overlay: &mut Overlay, what: &str, done: impl Fn(&Game, &Overlay) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        game.settle();
        overlay.settle();
        if done(game, overlay) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out: {what}\ngame: {:?}\noverlay: {:?}", game.client.state, overlay.client.state);
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn overlays_over_a_fullscreen_game() {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let env = vec![("RUST_LOG".to_owned(), "info".to_owned())];
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: None, size: (SIZE.0 as u32, SIZE.1 as u32), settings: "", env }) else { return };
    let display = session.wayland_display.clone();

    let mut game = Game::start(&display);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !(game.client.state.fullscreen && game.client.state.size == SIZE) {
        assert!(Instant::now() < deadline, "the game was not made full screen: {:?}", game.client.state);
        game.settle();
        std::thread::sleep(Duration::from_millis(20));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !game.focused() {
        assert!(Instant::now() < deadline, "the game has no keyboard: {:?}", game.client.state);
        game.settle();
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut overlay = Overlay::start(&display);
    let overlay = &mut overlay;
    until(&mut game, overlay, "the overlay's surfaces configured", |_, o| o.client.state.layer_configures.is_empty());

    // Shown: the HUD over the game; the bar hidden.
    let shown = snapshot(&session, "game-overlay-fullscreen");
    assert_eq!(rgb(&shown, HUD_SIZE.0 / 2, HUD_SIZE.1 / 2) & 0xFFFFFF, HUD & 0xFFFFFF, "the HUD is above the game");
    assert_eq!(rgb(&shown, SIZE.0 / 2, SIZE.1 / 2) & 0xFFFFFF, GAME & 0xFFFFFF, "the game");
    assert_eq!(rgb(&shown, SIZE.0 / 2, SIZE.1 - BAR_H / 2) & 0xFFFFFF, GAME & 0xFFFFFF, "the top-layer bar is hidden in full screen");
    // Neither exclusive zone shrank or moved the game, still full screen.
    assert!(game.client.state.fullscreen, "{:?}", game.client.state);
    assert_eq!(game.client.state.size, SIZE, "the exclusive zones don't shrink a full-screen window");
    assert_eq!(rgb(&shown, 2, SIZE.1 - 2) & 0xFFFFFF, GAME & 0xFFFFFF, "the game reaches the bottom-left corner");

    // The keyboard stays with the game.
    assert!(game.focused(), "the HUD did not take the keyboard: {:?}", game.client.state);
    assert_eq!(overlay.client.state.keyboard_enters, 0, "{:?}", overlay.client.state);
    session.input("tap 30");
    until(&mut game, overlay, "a key for the game", |g, _| g.client.state.keys.contains(&30));

    // The pointer goes through the HUD to the game.
    session.input(&format!("move {} {}\nbutton 272 1\nbutton 272 0", HUD_SIZE.0 / 2, HUD_SIZE.1 / 2));
    until(&mut game, overlay, "a click on the game under the HUD", |g, _| g.client.state.buttons == 1 && g.client.state.pointer_focus.as_ref() == Some(&g.surface.id()));
    assert_eq!(overlay.client.state.pointer_enters, 0, "the click-through HUD never has the pointer");

    // The HUD asks for the keyboard: it gets it; then gives it back.
    overlay.hud_keyboard(zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive);
    until(&mut game, overlay, "the HUD takes the keyboard", |g, o| o.hud_focused() && !g.focused());
    session.input("tap 31");
    until(&mut game, overlay, "a key for the HUD", |_, o| o.client.state.keys.contains(&31));
    overlay.hud_keyboard(zwlr_layer_surface_v1::KeyboardInteractivity::None);
    until(&mut game, overlay, "the game has the keyboard back", |g, o| !o.hud_focused() && g.focused());
    assert!(game.client.state.fullscreen && game.client.state.size == SIZE, "still full screen: {:?}", game.client.state);

    // While the HUD has the keyboard, a click on the game under it (the
    // HUD lets the pointer through) gives the game the keyboard, as a
    // click on the game anywhere else does.
    overlay.hud_keyboard(zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive);
    until(&mut game, overlay, "the HUD takes the keyboard again", |g, o| o.hud_focused() && !g.focused());
    session.input(&format!("move {} {}\nbutton 272 1\nbutton 272 0", HUD_SIZE.0 / 2, HUD_SIZE.1 / 2));
    until(&mut game, overlay, "a click under the HUD focuses the game", |g, o| g.focused() && !o.hud_focused());
    overlay.hud_keyboard(zwlr_layer_surface_v1::KeyboardInteractivity::None);

    // Out of full screen: the bar is back, the HUD still there.
    game.toplevel.unset_fullscreen();
    game.client.roundtrip();
    until(&mut game, overlay, "the game out of full screen", |g, _| !g.client.state.fullscreen);
    let windowed = snapshot(&session, "game-overlay-windowed");
    assert_eq!(rgb(&windowed, SIZE.0 / 2, SIZE.1 - BAR_H / 2) & 0xFFFFFF, BAR & 0xFFFFFF, "the bar is shown again");
    // The HUD's zone (a corner surface) reserves nothing: the bar spans
    // the whole bottom edge.
    assert_eq!(rgb(&windowed, 2, SIZE.1 - BAR_H / 2) & 0xFFFFFF, BAR & 0xFFFFFF, "a corner surface's exclusive zone is ignored");
    assert_eq!(rgb(&windowed, HUD_SIZE.0 / 2, HUD_SIZE.1 / 2) & 0xFFFFFF, HUD & 0xFFFFFF, "the HUD is still shown");
}
