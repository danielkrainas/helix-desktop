//! Windows chosen in the overview draw again and take input
//! (helixdesktop/spec `contracts/wayland/README.md`, "Shell surfaces and
//! the keyboard", "Frame callbacks"): in a headless Helix session two
//! maximized windows draw only when the compositor says a frame went by
//! (as browsers do: a new picture on each `wl_surface.frame` callback,
//! into whichever of two buffers the compositor has given back). One is
//! chosen in the overview while it is under the other, or minimized; it
//! must then keep drawing (new commits, its picture in the overview
//! current), have the keyboard and the pointer, and get a key, a click and
//! a scroll without the pointer moving first.
//!
//! An X11 window (`xev`) chosen the same way gets the key, the pointer, a
//! click and a wheel turn.
//!
//! Needs helix-comp built next to this test; skipped otherwise (and the X11
//! part without `xev`).

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};
use smithay_client_toolkit::reexports::client::globals::{registry_queue_init, GlobalListContents};
use smithay_client_toolkit::reexports::client::protocol::{wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// Window `n`'s colors: one per buffer, so every new picture differs.
const COLORS: [[u32; 2]; 2] = [[0xFF20_4080, 0xFF30_60C0], [0xFF80_4020, 0xFFC0_6030]];

#[derive(Default)]
struct State {
    keyboard: Option<wl_surface::WlSurface>,
    pointer: Option<wl_surface::WlSurface>,
    keys: Vec<(Option<wl_surface::WlSurface>, u32)>,
    buttons: Vec<(Option<wl_surface::WlSurface>, u32)>,
    scrolls: Vec<Option<wl_surface::WlSurface>>,
    configured: Vec<(xdg_surface::XdgSurface, u32)>,
    /// Per window: a frame went by (draw the next picture).
    frame_due: [bool; 2],
    /// Per window and buffer: the compositor gave it back.
    free: [[bool; 2]; 2],
    /// Per window: pictures committed.
    drawn: [u32; 2],
}

macro_rules! quiet {
    ($($iface:ty),*) => {$(
        impl Dispatch<$iface, ()> for State {
            fn event(_: &mut Self, _: &$iface, _: <$iface as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
quiet!(wl_compositor::WlCompositor, wl_surface::WlSurface, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_seat::WlSeat, xdg_toplevel::XdgToplevel);

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
            surface.ack_configure(serial);
            state.configured.push((surface.clone(), serial));
        }
    }
}
impl Dispatch<wl_callback::WlCallback, usize> for State {
    fn event(state: &mut Self, _: &wl_callback::WlCallback, event: wl_callback::Event, window: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = event {
            state.frame_due[*window] = true;
        }
    }
}
impl Dispatch<wl_buffer::WlBuffer, (usize, usize)> for State {
    fn event(state: &mut Self, _: &wl_buffer::WlBuffer, event: wl_buffer::Event, &(window, n): &(usize, usize), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_buffer::Event::Release = event {
            state.free[window][n] = true;
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(state: &mut Self, _: &wl_keyboard::WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_keyboard::Event::Enter { surface, .. } => state.keyboard = Some(surface),
            wl_keyboard::Event::Leave { .. } => state.keyboard = None,
            wl_keyboard::Event::Key { key, state: WEnum::Value(wl_keyboard::KeyState::Pressed), .. } => state.keys.push((state.keyboard.clone(), key)),
            _ => {}
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(state: &mut Self, _: &wl_pointer::WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_pointer::Event::Enter { surface, .. } => state.pointer = Some(surface),
            wl_pointer::Event::Leave { .. } => state.pointer = None,
            wl_pointer::Event::Button { button, state: WEnum::Value(wl_pointer::ButtonState::Pressed), .. } => state.buttons.push((state.pointer.clone(), button)),
            wl_pointer::Event::Axis { .. } => state.scrolls.push(state.pointer.clone()),
            _ => {}
        }
    }
}

/// A window drawing a new picture on every frame callback.
struct Drawing {
    surface: wl_surface::WlSurface,
    _xdg: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    buffers: [wl_buffer::WlBuffer; 2],
    /// The buffer the next picture goes into.
    next: usize,
}

struct Client {
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    base: xdg_wm_base::XdgWmBase,
    scratch: std::path::PathBuf,
    windows: Vec<Drawing>,
}

impl Client {
    /// Read events, and draw the windows whose frame went by.
    fn pump(&mut self) {
        self.queue.roundtrip(&mut self.state).unwrap();
        for (i, w) in self.windows.iter_mut().enumerate() {
            if !self.state.frame_due[i] {
                continue;
            }
            // Into a buffer the compositor gave back (both are ours at
            // first); none free: the next frame, then.
            let Some(n) = [w.next, 1 - w.next].into_iter().find(|n| self.state.free[i][*n]) else { continue };
            self.state.frame_due[i] = false;
            self.state.free[i][n] = false;
            w.surface.attach(Some(&w.buffers[n]), 0, 0);
            w.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
            w.surface.frame(&self.qh, i);
            w.surface.commit();
            w.next = 1 - n;
            self.state.drawn[i] += 1;
        }
        let _ = self.queue.flush();
    }

    fn wait(&mut self, timeout: Duration, done: impl Fn(&State) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            self.pump();
            if done(&self.state) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn buffer(&self, (w, h): (i32, i32), color: u32, data: (usize, usize)) -> wl_buffer::WlBuffer {
        let bytes: Vec<u8> = std::iter::repeat_n(color.to_le_bytes(), (w * h) as usize).flatten().collect();
        let path = self.scratch.join(format!("buffer-{color:08x}"));
        std::fs::write(&path, &bytes).unwrap();
        let file = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let pool = self.shm.create_pool(file.as_fd(), bytes.len() as i32, &self.qh, ());
        let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, &self.qh, data);
        pool.destroy();
        buffer
    }

    /// A maximized window titled `title` that starts drawing.
    fn drawing(&mut self, title: &str, size: (i32, i32)) {
        let i = self.windows.len();
        let surface = self.compositor.create_surface(&self.qh, ());
        let xdg = self.base.get_xdg_surface(&surface, &self.qh, ());
        let toplevel = xdg.get_toplevel(&self.qh, ());
        toplevel.set_title(title.to_owned());
        toplevel.set_app_id(format!("org.helixdesktop.Test{title}"));
        toplevel.set_maximized();
        surface.commit();
        let x = xdg.clone();
        assert!(self.wait(Duration::from_secs(5), |s| s.configured.iter().any(|(c, _)| c == &x)), "no first configure");
        let buffers = [self.buffer(size, COLORS[i][0], (i, 0)), self.buffer(size, COLORS[i][1], (i, 1))];
        self.state.free[i] = [true, true];
        // The first picture; then one per frame callback.
        self.state.frame_due[i] = true;
        self.windows.push(Drawing { surface, _xdg: xdg, toplevel, buffers, next: 0 });
        self.pump();
    }
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

/// The tiles of the last "overview tiles" line: (title, center).
fn tiles(log: &str) -> Vec<(String, (i32, i32))> {
    let Some(line) = log.lines().rfind(|l| l.contains("overview tiles")) else { return Vec::new() };
    let Some(list) = line.split("tiles=").nth(1) else { return Vec::new() };
    list.trim_matches(|c: char| c == '"' || c.is_whitespace())
        .split(' ')
        .filter_map(|w| {
            let (title, rect) = w.split_once('@')?;
            let n: Vec<i32> = rect.trim_matches('"').split(',').map(|v| v.parse().ok()).collect::<Option<_>>()?;
            (n.len() == 4).then(|| (title.to_owned(), (n[0] + n[2] / 2, n[1] + n[3] / 2)))
        })
        .collect()
}

fn start(env: Vec<(String, String)>) -> Option<HeadlessSession> {
    let comp = sibling_binary("helix-comp").or_else(|| {
        eprintln!("helix-comp is not built next to this test; skipped");
        None
    })?;
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    let settings = "[org/gnome/desktop/interface]\nenable-hot-corners=false\n";
    let mut env = env;
    env.push(("RUST_LOG".to_owned(), "info,helix_shell::ui::overview=debug,helix_comp::compositor::capture=debug".to_owned()));
    let session = HeadlessSession::start(Options { comp: &comp, shell: Some(&shell), size: (1280, 800), settings, env })?;
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());
    Some(session)
}

fn connect(session: &HeadlessSession) -> Client {
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
        queue,
        qh,
        state: State::default(),
        scratch: session.home().join("tmp"),
        windows: Vec::new(),
    }
}

/// Open the overview, wait for `count` tiles, and click `title`'s.
fn choose_in_overview(session: &mut HeadlessSession, c: &mut Client, title: &str, count: usize) -> (i32, i32) {
    let drawn_before = session.log().lines().filter(|l| l.contains("overview tiles")).count();
    overview(session, true);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let log = session.log();
        if log.lines().filter(|l| l.contains("overview tiles")).count() > drawn_before && tiles(&log).len() == count {
            break;
        }
        assert!(Instant::now() < deadline, "no tiles:\n{log}");
        c.wait(Duration::from_millis(50), |_| false);
    }
    // The pictures come in; they renew while the overview is open.
    c.wait(Duration::from_millis(1200), |_| false);
    let (x, y) = tiles(&session.log()).iter().find(|(t, _)| t.starts_with(title)).unwrap_or_else(|| panic!("no tile for {title}: {:?}", tiles(&session.log()))).1;
    session.input(&format!("move {x} {y}"));
    session.input("button 272 1");
    c.wait(Duration::from_millis(40), |_| false);
    session.input("button 272 0");
    (x, y)
}

/// The chosen window `i` draws again and takes input where the pointer
/// is: what failed, if anything.
fn check_chosen(session: &mut HeadlessSession, c: &mut Client, i: usize, what: &str) -> Option<String> {
    let target = c.windows[i].surface.clone();
    let t = target.clone();
    let focused = c.wait(Duration::from_secs(3), |st| st.keyboard.as_ref() == Some(&t));
    // It keeps drawing: a frame callback per frame shown (60 a second).
    let before = c.state.drawn[i];
    c.wait(Duration::from_millis(500), |_| false);
    let frames = c.state.drawn[i] - before;
    let drawing = frames >= 10;
    let keys = c.state.keys.len();
    session.input("tap 30");
    let t = target.clone();
    let keyed = c.wait(Duration::from_secs(2), |st| st.keys[keys..].iter().any(|(s, k)| s.as_ref() == Some(&t) && *k == 30));
    let t = target.clone();
    let entered = c.wait(Duration::from_secs(2), |st| st.pointer.as_ref() == Some(&t));
    let scrolls = c.state.scrolls.len();
    session.input("scroll 0 15");
    let t = target.clone();
    let scrolled = c.wait(Duration::from_secs(2), |st| st.scrolls[scrolls..].iter().any(|s| s.as_ref() == Some(&t)));
    let buttons = c.state.buttons.len();
    session.input("button 272 1\nbutton 272 0");
    let t = target.clone();
    let clicked = c.wait(Duration::from_secs(2), |st| st.buttons[buttons..].iter().any(|(s, b)| s.as_ref() == Some(&t) && *b == 0x110));
    let outcome = format!("{what}: focused {focused}, drawing {drawing} ({frames} pictures in 0.5 s), key {keyed}, pointer entered {entered}, scroll {scrolled}, click {clicked}");
    eprintln!("{outcome}");
    (!(focused && drawing && keyed && entered && scrolled && clicked)).then_some(outcome)
}

#[test]
fn windows_chosen_in_the_overview_draw_and_take_input() {
    let Some(mut session) = start(Vec::new()) else { return };
    let mut c = connect(&session);
    c.drawing("First", (1280, 768));
    c.drawing("Second", (1280, 768));
    let s = c.windows[1].surface.clone();
    assert!(c.wait(Duration::from_secs(5), |st| st.keyboard.as_ref() == Some(&s)), "the new window has no focus");
    // Both draw, the one underneath too.
    let before = c.state.drawn;
    c.wait(Duration::from_millis(500), |_| false);
    assert!(c.state.drawn[0] - before[0] >= 10 && c.state.drawn[1] - before[1] >= 10, "the windows do not draw on frame callbacks: {:?} -> {:?}", before, c.state.drawn);

    let mut failures = Vec::new();
    for round in 0..6 {
        // The window under the other one (it went on drawing there).
        let i = if c.state.keyboard.as_ref() == Some(&c.windows[0].surface) { 1 } else { 0 };
        let title = ["First", "Second"][i];
        let (x, y) = choose_in_overview(&mut session, &mut c, title, 2);
        failures.extend(check_chosen(&mut session, &mut c, i, &format!("round {round}, {title} from under the other")));
        session.input(&format!("move {} {}", x + 2, y + 2));
        c.wait(Duration::from_millis(200), |_| false);
    }

    // Minimized: chosen in the overview, it comes back and draws.
    for round in 0..3 {
        let i = if c.state.keyboard.as_ref() == Some(&c.windows[0].surface) { 0 } else { 1 };
        let title = ["First", "Second"][i];
        c.windows[i].toplevel.set_minimized();
        let other = c.windows[1 - i].surface.clone();
        assert!(c.wait(Duration::from_secs(3), |st| st.keyboard.as_ref() == Some(&other)), "minimizing did not focus the other window");
        c.wait(Duration::from_millis(300), |_| false);
        let (x, y) = choose_in_overview(&mut session, &mut c, title, 2);
        failures.extend(check_chosen(&mut session, &mut c, i, &format!("round {round}, {title} minimized")));
        session.input(&format!("move {} {}", x + 2, y + 2));
        c.wait(Duration::from_millis(200), |_| false);
    }
    let log = session.log();
    assert!(failures.is_empty(), "chosen windows did not draw or get input:\n{}\n\n{log}", failures.join("\n"));
    assert!(!log.contains("panicked"), "{log}");
}

#[test]
fn x11_window_chosen_in_the_overview_takes_input() {
    if !std::path::Path::new("/usr/bin/xev").exists() {
        eprintln!("no xev; skipped");
        return;
    }
    let Some(mut session) = start(Vec::new()) else { return };
    assert!(session.x_display != ":0" && session.x_display != ":1", "not the session's own Xwayland: {}", session.x_display);
    let mut c = connect(&session);
    let out = session.home().join("xev.log");
    let child = std::process::Command::new("/usr/bin/xev")
        .args(["-name", "Xev", "-geometry", "1280x768+0+0", "-event", "keyboard", "-event", "button", "-event", "mouse"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", session.home())
        .env("DISPLAY", &session.x_display)
        .env("XAUTHORITY", &session.x_authority)
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(&out).unwrap())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _xev = Kill(child);
    assert!(session.wait_for_log("new X11 window", Duration::from_secs(10)), "xev did not open:\n{}", session.log());
    c.wait(Duration::from_millis(500), |_| false);
    // A Wayland window over it.
    c.drawing("Cover", (1280, 768));
    let s = c.windows[0].surface.clone();
    assert!(c.wait(Duration::from_secs(5), |st| st.keyboard.as_ref() == Some(&s)), "the new window has no focus");

    let read = || std::fs::read_to_string(&out).unwrap_or_default();
    let mut failures = Vec::new();
    for round in 0..4 {
        let (x, y) = choose_in_overview(&mut session, &mut c, "Xev", 2);
        let t = s.clone();
        let left = c.wait(Duration::from_secs(3), |st| st.keyboard.as_ref() != Some(&t));
        c.wait(Duration::from_millis(500), |_| false);
        let seen = read().len();
        session.input("tap 30");
        c.wait(Duration::from_millis(400), |_| false);
        session.input("wheel 1");
        c.wait(Duration::from_millis(400), |_| false);
        session.input("button 272 1\nbutton 272 0");
        c.wait(Duration::from_millis(400), |_| false);
        let new = read()[seen..].to_owned();
        let keyed = new.contains("KeyPress event") && new.contains("keycode 38");
        let wheeled = new.contains("button 5,");
        let clicked = new.contains("button 1,");
        let outcome = format!("round {round}: the cover lost the keyboard {left}, key {keyed}, wheel {wheeled}, click {clicked}");
        eprintln!("{outcome}");
        if !(left && keyed && wheeled && clicked) {
            failures.push(format!("{outcome}\n{new}"));
        }
        // Back to the cover.
        let (cx, cy) = choose_in_overview(&mut session, &mut c, "Cover", 2);
        let t = s.clone();
        c.wait(Duration::from_secs(3), |st| st.keyboard.as_ref() == Some(&t));
        session.input(&format!("move {} {}", cx.min(x) + 3, cy.min(y) + 3));
        c.wait(Duration::from_millis(200), |_| false);
    }
    let log = session.log();
    assert!(failures.is_empty(), "the X11 window did not get input:\n{}\n\n{log}", failures.join("\n"));
    assert!(!log.contains("panicked"), "{log}");
}

