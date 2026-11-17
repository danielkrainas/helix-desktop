//! Switching windows from the overview (helixdesktop/spec
//! `contracts/wayland/README.md`, "Shell surfaces and the keyboard"): in a
//! headless Helix session (helix-comp with helix-shell, scratch home,
//! private bus) a client opens two maximized windows, the overview is
//! opened, and a real click (press and release through the compositor) on
//! the other window's tile switches to it. The chosen window then has the
//! keyboard and gets a key, and the pointer, which is now over it, works
//! at once: a click and a scroll there reach it without the pointer moving
//! first. Repeated with different pauses between the press and the
//! release and after the release, with and without a little motion right
//! after the release (while the shell is still closing the overview).
//!
//! Before the fix the pointer stayed on the overview's destroyed surface
//! until it moved: the chosen window got no pointer `enter`, and clicks
//! and scrolls went nowhere (every round failed).
//!
//! The dash's icons answer as the dock's do (`dash_icons_answer_like_the_dock`):
//! resting the pointer on one lights it (drawn again frame by frame while
//! the highlight fades in) and shows its name above it, pressing darkens
//! the highlight, and releasing on it goes to the app's window.
//!
//! The tiles and dash icons are found from the shell's debug log
//! ("overview tiles", "overview dash").
//! Needs helix-comp built next to this test; skipped otherwise.

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};
use smithay_client_toolkit::reexports::client::globals::{registry_queue_init, GlobalListContents};
use smithay_client_toolkit::reexports::client::protocol::{wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use smithay_client_toolkit::reexports::protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
struct State {
    /// The surface with keyboard focus.
    keyboard: Option<wl_surface::WlSurface>,
    /// The surface under the pointer.
    pointer: Option<wl_surface::WlSurface>,
    /// Keys pressed, with the surface that had the keyboard.
    keys: Vec<(Option<wl_surface::WlSurface>, u32)>,
    /// Buttons pressed, with the surface under the pointer.
    buttons: Vec<(Option<wl_surface::WlSurface>, u32)>,
    /// Scrolls, with the surface under the pointer.
    scrolls: Vec<Option<wl_surface::WlSurface>>,
    /// Pending configure serials, per xdg_surface.
    configured: Vec<(xdg_surface::XdgSurface, u32)>,
}

macro_rules! quiet {
    ($($iface:ty),*) => {$(
        impl Dispatch<$iface, ()> for State {
            fn event(_: &mut Self, _: &$iface, _: <$iface as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
quiet!(wl_compositor::WlCompositor, wl_surface::WlSurface, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, wl_seat::WlSeat, xdg_toplevel::XdgToplevel);

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
            // Acked at once: these windows draw nothing new.
            surface.ack_configure(serial);
            state.configured.push((surface.clone(), serial));
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

struct Client {
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    base: xdg_wm_base::XdgWmBase,
    scratch: std::path::PathBuf,
}

struct Toplevel {
    surface: wl_surface::WlSurface,
    _xdg: xdg_surface::XdgSurface,
    _toplevel: xdg_toplevel::XdgToplevel,
}

impl Client {
    fn roundtrip(&mut self) {
        self.queue.roundtrip(&mut self.state).unwrap();
    }

    /// Run until `done` holds, or `timeout` passes; whether it held.
    fn wait(&mut self, timeout: Duration, done: impl Fn(&State) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            self.roundtrip();
            if done(&self.state) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn buffer(&self, (w, h): (i32, i32), color: u32) -> wl_buffer::WlBuffer {
        let bytes: Vec<u8> = std::iter::repeat_n(color.to_le_bytes(), (w * h) as usize).flatten().collect();
        let path = self.scratch.join(format!("buffer-{color:08x}"));
        std::fs::write(&path, &bytes).unwrap();
        let file = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let pool = self.shm.create_pool(file.as_fd(), bytes.len() as i32, &self.qh, ());
        let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, &self.qh, ());
        pool.destroy();
        buffer
    }

    /// A mapped toplevel titled `title`.
    fn toplevel(&mut self, title: &str, size: (i32, i32), color: u32) -> Toplevel {
        let surface = self.compositor.create_surface(&self.qh, ());
        let xdg = self.base.get_xdg_surface(&surface, &self.qh, ());
        let toplevel = xdg.get_toplevel(&self.qh, ());
        toplevel.set_title(title.to_owned());
        toplevel.set_app_id(format!("org.helixdesktop.Test{title}"));
        // Maximized: the chosen window is under its tile once it is raised.
        toplevel.set_maximized();
        surface.commit();
        let x = xdg.clone();
        assert!(self.wait(Duration::from_secs(5), |s| s.configured.iter().any(|(c, _)| c == &x)), "no first configure");
        surface.attach(Some(&self.buffer(size, color)), 0, 0);
        surface.damage_buffer(0, 0, size.0, size.1);
        surface.commit();
        self.roundtrip();
        Toplevel { surface, _xdg: xdg, _toplevel: toplevel }
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

#[test]
fn overview_switches_windows_with_working_input() {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return;
    };
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    let settings = "[org/gnome/desktop/interface]\nenable-hot-corners=false\n";
    let env = vec![("RUST_LOG".to_owned(), "info,helix_shell::ui::overview=debug".to_owned())];
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: Some(&shell), size: (1280, 800), settings, env }) else { return };
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());

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
        queue,
        qh,
        state: State::default(),
        scratch: session.home().join("tmp"),
    };
    let first = c.toplevel("First", (1280, 768), 0xFF20_4080);
    let second = c.toplevel("Second", (1280, 768), 0xFF80_4020);
    let s = second.surface.clone();
    assert!(c.wait(Duration::from_secs(5), |st| st.keyboard.as_ref() == Some(&s)), "the new window has no focus");
    let windows = [("First", first.surface.clone()), ("Second", second.surface.clone())];

    // (pause between press and release, pause after the release, a little
    // motion right after the release) in ms.
    let timings: &[(u64, u64, bool)] = &[(0, 0, false), (0, 300, false), (5, 0, true), (20, 0, false), (50, 100, true), (100, 0, false), (150, 0, true), (300, 0, false), (20, 300, true), (0, 0, true), (80, 0, false), (200, 50, true)];
    let mut failures = Vec::new();
    for (round, &(hold, after, jiggle)) in timings.iter().enumerate() {
        // Switch to the window that does not have focus now.
        c.roundtrip();
        let (title, target) = windows.iter().find(|(_, w)| c.state.keyboard.as_ref() != Some(w)).cloned().unwrap();
        let drawn_before = session.log().lines().filter(|l| l.contains("overview tiles")).count();
        overview(&session, true);
        // The overview takes the keyboard, and shows both windows.
        assert!(c.wait(Duration::from_secs(5), |st| st.keyboard.is_none()), "round {round}: the overview did not take the keyboard");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let log = session.log();
            if log.lines().filter(|l| l.contains("overview tiles")).count() > drawn_before && tiles(&log).len() == 2 {
                break;
            }
            assert!(Instant::now() < deadline, "round {round}: no tiles:\n{log}");
            std::thread::sleep(Duration::from_millis(50));
        }
        // A little time for pictures to come in and the tiles to settle.
        std::thread::sleep(Duration::from_millis(300));
        let (x, y) = tiles(&session.log()).iter().find(|(t, _)| t == title).unwrap().1;

        session.input(&format!("move {x} {y}"));
        session.input("button 272 1");
        std::thread::sleep(Duration::from_millis(hold));
        session.input("button 272 0");
        if jiggle {
            // Motion while the shell is still closing the overview.
            session.input(&format!("move {} {}", x + 1, y));
        }
        std::thread::sleep(Duration::from_millis(after));

        let t = target.clone();
        let focused = c.wait(Duration::from_secs(3), |st| st.keyboard.as_ref() == Some(&t));
        // A key reaches the chosen window.
        let keys = c.state.keys.len();
        session.input("tap 30");
        let t = target.clone();
        let keyed = c.wait(Duration::from_secs(2), |st| st.keys[keys..].iter().any(|(s, k)| s.as_ref() == Some(&t) && *k == 30));
        // The pointer is over it now (it fills the screen): a scroll and a
        // click there reach it, without moving the pointer first.
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
        let outcome = format!("round {round} ({title}, hold {hold} ms, after {after} ms, motion {jiggle}): focused {focused}, key {keyed}, pointer entered {entered}, scroll {scrolled}, click {clicked}");
        eprintln!("{outcome}");
        if !(focused && keyed && entered && scrolled && clicked) {
            failures.push(outcome);
        }
        // The next round starts from the chosen window having the keyboard
        // and the pointer (moving puts the pointer on it however this
        // round went).
        session.input(&format!("move {} {}", x + 2, y + 2));
        let t = target.clone();
        c.wait(Duration::from_secs(2), |st| st.keyboard.as_ref() == Some(&t));
        overview(&session, false);
        std::thread::sleep(Duration::from_millis(200));
    }
    let log = session.log();
    assert!(failures.is_empty(), "the chosen window did not get input:\n{}\n\n{log}", failures.join("\n"));
    assert!(!log.contains("panicked"), "{log}");
}

/// The dash icons of the last "overview dash" line: (app, x, y, w, h).
fn dash_icons(log: &str) -> Vec<(String, [i32; 4])> {
    let Some(line) = log.lines().rfind(|l| l.contains("overview dash")) else { return Vec::new() };
    let Some(list) = line.split("dash=").nth(1) else { return Vec::new() };
    list.trim_matches(|c: char| c == '"' || c.is_whitespace())
        .split(' ')
        .filter_map(|w| {
            let (app, rect) = w.split_once('@')?;
            let n: Vec<i32> = rect.trim_matches('"').split(',').map(|v| v.parse().ok()).collect::<Option<_>>()?;
            (n.len() == 4).then(|| (app.to_owned(), [n[0], n[1], n[2], n[3]]))
        })
        .collect()
}

/// The mean brightness (0 to 255) of a snapshot's `x0..x1`×`y0..y1`.
fn brightness((w, data): (u32, &[u8]), (x0, x1): (i32, i32), (y0, y1): (i32, i32)) -> f64 {
    let mut sum = 0u64;
    let mut n = 0u64;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y as u32 * w + x as u32) * 4) as usize;
            sum += u64::from(data[i]) + u64::from(data[i + 1]) + u64::from(data[i + 2]);
            n += 3;
        }
    }
    sum as f64 / n.max(1) as f64
}

/// The times (ms) of the log's drawings whose message is `what`
/// ("overview drawn": the whole overview; "overview dash drawn": the dash
/// alone).
fn draws(log: &str, what: &str) -> Vec<f64> {
    log.lines().filter(|l| l.contains(what)).filter_map(|l| l.split("ms=").nth(1)?.split_whitespace().next()?.parse().ok()).collect()
}

#[test]
fn dash_icons_answer_like_the_dock() {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return;
    };
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    let settings = "[org/gnome/desktop/interface]\nenable-hot-corners=false\nenable-animations=true\n";
    let env = vec![("RUST_LOG".to_owned(), "info,helix_shell::ui::overview=debug".to_owned())];
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: Some(&shell), size: (1280, 800), settings, env }) else { return };
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());

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
        queue,
        qh,
        state: State::default(),
        scratch: session.home().join("tmp"),
    };
    let first = c.toplevel("First", (1280, 768), 0xFF20_4080);
    let _second = c.toplevel("Second", (1280, 768), 0xFF80_4020);

    overview(&session, true);
    assert!(c.wait(Duration::from_secs(5), |st| st.keyboard.is_none()), "the overview did not take the keyboard");
    let deadline = Instant::now() + Duration::from_secs(10);
    let first_icon = |log: &str| dash_icons(log).into_iter().find(|(app, _)| app == "org.helixdesktop.TestFirst").map(|(_, r)| r);
    while first_icon(&session.log()).is_none() {
        assert!(Instant::now() < deadline, "no dash icon for First:\n{}", session.log());
        std::thread::sleep(Duration::from_millis(50));
    }
    // A little time for the overview to settle (its first drawing can be
    // at another size), then where the icon is now.
    std::thread::sleep(Duration::from_millis(500));
    let icon = first_icon(&session.log()).unwrap();
    let [x, y, w, h] = icon;
    // The ring between the icon's edge and the highlight's (4 px around),
    // and the place above the icon where its name shows.
    let ring = |shot: (u32, &[u8])| brightness(shot, (x - 3, x - 1), (y + 8, y + h - 8)) + brightness(shot, (x + w + 1, x + w + 3), (y + 8, y + h - 8));
    let above = |shot: (u32, &[u8])| brightness(shot, (x + 4, x + w - 4), (y - 30, y - 18));
    // Rest the pointer away from the dash first.
    session.input("move 640 300");
    std::thread::sleep(Duration::from_millis(500));
    let (sw, _, before) = session.fresh_snapshot();

    // Hover: the dash alone is drawn again frame by frame while the
    // highlight fades in, not the whole overview.
    let log = session.log();
    let (full, dash) = (draws(&log, "overview drawn").len(), draws(&log, "overview dash drawn").len());
    session.input(&format!("move {} {}", x + w / 2, y + h / 2));
    std::thread::sleep(Duration::from_millis(700));
    let log = session.log();
    let (full_times, dash_times) = (draws(&log, "overview drawn"), draws(&log, "overview dash drawn"));
    let frames = &dash_times[dash..];
    eprintln!("hover: {} dash draws (slowest {:.1} ms, mean {:.1} ms), {} whole draws", frames.len(), frames.iter().copied().fold(0.0, f64::max), frames.iter().sum::<f64>() / frames.len().max(1) as f64, full_times.len() - full);
    assert!(frames.len() >= 6, "the fade was not drawn frame by frame: {frames:?}\n{}", log.lines().filter(|l| l.contains("dash=") || l.contains("overview tiles")).collect::<Vec<_>>().join("\n"));
    assert_eq!(full_times.len(), full, "hovering the dash drew the whole overview");
    let (_, _, lit) = session.fresh_snapshot();
    let (ring_rest, ring_lit) = (ring((sw, &before)), ring((sw, &lit)));
    let (above_rest, above_lit) = (above((sw, &before)), above((sw, &lit)));
    eprintln!("ring: rest {ring_rest:.1}, lit {ring_lit:.1}; above: rest {above_rest:.1}, tooltip {above_lit:.1}");
    assert!(ring_lit > ring_rest + 10.0, "no highlight behind the hovered icon");
    assert!((above_lit - above_rest).abs() > 5.0, "no name above the hovered icon");

    // Press: a darker (stronger) highlight; no tooltip while pressed.
    session.input("button 272 1");
    std::thread::sleep(Duration::from_millis(400));
    let (_, _, pressed) = session.fresh_snapshot();
    let ring_pressed = ring((sw, &pressed));
    eprintln!("ring pressed {ring_pressed:.1}");
    assert!(ring_pressed > ring_lit + 3.0, "pressing did not change the highlight");

    // Release on it: its window comes up and the overview closes.
    session.input("button 272 0");
    let f = first.surface.clone();
    assert!(c.wait(Duration::from_secs(3), |st| st.keyboard.as_ref() == Some(&f)), "releasing on the icon did not go to First's window");
    let log = session.log();
    assert!(!log.contains("panicked"), "{log}");
}
