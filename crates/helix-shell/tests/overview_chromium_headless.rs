//! A Chromium-based browser (Google Chrome or Brave, if installed) switched
//! to from the overview keeps drawing and takes input, on Wayland (ozone)
//! and through Xwayland. Opt-in: it runs a browser installed on the system,
//! so only with `HELIX_TEST_CHROMIUM=1`, in a scratch profile in the
//! session's scratch home.
//!
//! A page animates its top half every frame and colors its bottom half by
//! the last input (key green, wheel red, click blue). A test window covers
//! the browser for a while; then the browser is chosen in the overview and
//! the snapshots must show it animating and answering a key, a scroll and
//! a click without the pointer moving.
//!
//! - `HELIX_TEST_GPU=/dev/dri/renderD…`: the headless compositor draws with
//!   that GPU and offers GPU buffers (and explicit sync), so the browser
//!   draws with the GPU as on hardware.
//! - `HELIX_TEST_OUTPUTS=2`: two monitors, the browser on the second.
//! - `HELIX_TEST_MENU=1`: a context menu open in the browser when the
//!   overview opens; `HELIX_TEST_SUPER=1`: the overview opened with Super.
//! - `HELIX_TEST_TABS=1`: a normal browser window (tabs, and the
//!   minimize, maximize and close buttons) instead of an app window;
//!   with `HELIX_TEST_MINIMIZE=1` the browser minimizes itself (its
//!   minimize button) instead of being covered, then is chosen in the
//!   overview. Before the compositor deactivated hidden windows it stayed
//!   a still picture that took no input (every round).
//! - `HELIX_TEST_ROUNDS`, `HELIX_TEST_HIDDEN` (s), `HELIX_TEST_DWELL` (ms),
//!   `HELIX_TEST_SHOT=file.png` (the browser's monitor when it is up).
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};
use smithay_client_toolkit::reexports::client::globals::{registry_queue_init, GlobalListContents};
use smithay_client_toolkit::reexports::client::protocol::{wl_buffer, wl_compositor, wl_region, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface};
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
quiet!(wl_region::WlRegion, wl_compositor::WlCompositor, wl_surface::WlSurface, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, wl_seat::WlSeat, xdg_toplevel::XdgToplevel);

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
        // Opaque, as most apps' windows are: what is under it is hidden
        // for the compositor too.
        let region = self.compositor.create_region(&self.qh, ());
        region.add(0, 0, size.0, size.1);
        surface.set_opaque_region(Some(&region));
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


/// A Chromium-based browser on the system, if any.
fn chrome() -> Option<&'static str> {
    ["/usr/bin/google-chrome", "/usr/bin/brave-browser"].into_iter().find(|p| std::path::Path::new(p).exists())
}

fn rgb(data: &[u8], w: u32, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * w + x) * 4) as usize;
    [data[i], data[i + 1], data[i + 2]]
}

const PAGE: &str = r#"<!doctype html><html><head><title>Chrome</title></head>
<body style="margin:0;overflow:hidden"><div id=a style="height:50vh"></div><div id=b style="height:50vh;background:#808080"></div>
<script>
let n=0; function f(){n++; a.style.background=`rgb(${(n*3)%256},${(n*7)%256},${(n*13)%256})`; requestAnimationFrame(f)} f();
addEventListener('keydown',e=>{b.style.background='#00ff00'});
addEventListener('mousedown',e=>{b.style.background='#0000ff'});
addEventListener('wheel',e=>{b.style.background='#ff0000'});
</script></body></html>"#;

/// A fresh snapshot, answering the compositor meanwhile.
fn snap(session: &HeadlessSession, c: &mut Client) -> (u32, u32, Vec<u8>) {
    let second = std::env::var("HELIX_TEST_OUTPUTS").is_ok_and(|v| v == "2");
    let path = if second { session.home().join("snapshot-2.png") } else { session.snapshot_path() };
    let since = std::time::SystemTime::now();
    c.wait(Duration::from_secs(10), |_| std::fs::metadata(&path).and_then(|m| m.modified()).is_ok_and(|t| t > since));
    c.wait(Duration::from_millis(1100), |_| false);
    let bytes = std::fs::read(&path).unwrap();
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).unwrap();
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

/// Tiles of the primary monitor's overview and the other monitor's.
fn all_tiles(log: &str) -> Vec<(String, (i32, i32))> {
    let mut out = tiles(log);
    let other = log.lines().rfind(|l| l.contains("overview other tiles")).unwrap_or("").replace("overview other tiles", "overview tiles");
    out.extend(tiles(&other));
    out
}

fn explore(ozone: &str) {
    let Some(comp) = sibling_binary("helix-comp") else { return };
    let Some(browser) = chrome() else { eprintln!("no chrome"); return };
    if std::env::var_os("HELIX_TEST_CHROMIUM").is_none() { eprintln!("HELIX_TEST_CHROMIUM unset; skipped"); return; }
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    let settings = "[org/gnome/desktop/interface]\nenable-hot-corners=false\n\n[org/gnome/desktop/wm/preferences]\nbutton-layout='appmenu:minimize,maximize,close'\n";
    let mut env = vec![("RUST_LOG".to_owned(), std::env::var("HELIX_TEST_RUST_LOG").unwrap_or("info,helix_shell::ui::overview=debug".to_owned()))];
    if let Ok(gpu) = std::env::var("HELIX_TEST_GPU") {
        env.push(("HELIX_HEADLESS_GPU".to_owned(), gpu));
    }
    let two = std::env::var("HELIX_TEST_OUTPUTS").is_ok_and(|v| v == "2");
    if two {
        env.push(("HELIX_HEADLESS_OUTPUTS".to_owned(), "2".to_owned()));
    }
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: Some(&shell), size: (1280, 800), settings, env }) else { return };
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());
    assert!(session.x_display != ":0" && session.x_display != ":1", "{}", session.x_display);

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
    let other = c.toplevel("Other", (1280, 768), 0xFF20_4080);

    if two {
        // Chrome opens on the second monitor (where the pointer is).
        session.input("move 1920 400");
    }
    let page = session.home().join("page.html");
    std::fs::write(&page, PAGE).unwrap();
    let profile = session.home().join("chrome-profile");
    let log = std::fs::File::create(session.home().join("chrome.log")).unwrap();
    let mut cmd = std::process::Command::new(browser);
    cmd.args([format!("--user-data-dir={}", profile.display()), "--no-first-run".into(), "--no-default-browser-check".into(), "--disable-sync".into(), "--password-store=basic".into(), "--start-maximized".into(), format!("--ozone-platform={ozone}"), if std::env::var_os("HELIX_TEST_TABS").is_some() { format!("file://{}", page.display()) } else { format!("--app=file://{}", page.display()) }])
        .args(std::env::var("HELIX_TEST_CHROME_ARGS").unwrap_or_default().split_whitespace())
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", session.home())
        .env("XDG_CONFIG_HOME", session.home().join(".config"))
        .env("XDG_DATA_HOME", session.home().join(".local/share"))
        .env("XDG_CACHE_HOME", session.home().join(".cache"))
        .env("XDG_STATE_HOME", session.home().join(".local/state"))
        .env("XDG_RUNTIME_DIR", session.home().join("run"))
        .env("TMPDIR", session.home().join("tmp"))
        .env("DBUS_SESSION_BUS_ADDRESS", session.bus().address())
        .env("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent/helix-test")
        .env("GSETTINGS_BACKEND", "keyfile")
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(log);
    if ozone == "wayland" {
        cmd.env("WAYLAND_DISPLAY", &session.wayland_display);
    } else {
        cmd.env("DISPLAY", &session.x_display).env("XAUTHORITY", &session.x_authority);
    }
    struct Kill(std::process::Child);
    impl Drop for Kill { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
    let _guard = Kill(cmd.spawn().unwrap());

    // Chrome comes up and takes the keyboard.
    let o = other.surface.clone();
    assert!(c.wait(Duration::from_secs(30), |st| st.keyboard.as_ref() != Some(&o)), "chrome did not take the keyboard:\n{}", session.log());
    c.wait(Duration::from_secs(3), |_| false);
    let probe = |session: &HeadlessSession, c: &mut Client, what: &str| {
        let (w, _, a) = snap(session, c);
        let (_, _, b) = snap(session, c);
        let (top_a, top_b, bottom) = (rgb(&a, w, 640, 200), rgb(&b, w, 640, 200), rgb(&b, w, 640, 600));
        eprintln!("{what}: top {top_a:?} -> {top_b:?}, bottom {bottom:?}");
        (top_a != top_b, bottom)
    };
    let (live, _) = probe(&session, &mut c, "chrome up");
    if let Ok(dest) = std::env::var("HELIX_TEST_SHOT") {
        let _ = std::fs::copy(if two { session.home().join("snapshot-2.png") } else { session.snapshot_path() }, dest);
    }
    assert!(live, "chrome is not animating at all");
    for amount in ["0 15", "0 150", "0 -150"] {
        session.input(&format!("scroll {amount}"));
        c.wait(Duration::from_millis(500), |_| false);
        probe(&session, &mut c, &format!("before overview, scroll {amount}"));
        session.input("tap 30");
        c.wait(Duration::from_millis(500), |_| false);
        probe(&session, &mut c, "key");
    }

    let rounds: usize = std::env::var("HELIX_TEST_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
    let mut failures = Vec::new();
    for round in 0..rounds {
        // To Other through the overview.
        for (title, to_chrome) in [("Other", false), ("Chrome", true)] {
            let drawn_before = session.log().lines().filter(|l| l.contains("overview tiles")).count();
            if !to_chrome && std::env::var_os("HELIX_TEST_MINIMIZE").is_some() {
                // Chrome minimizes itself (its minimize button, shown with
                // `button-layout`; HELIX_TEST_TABS=1 for a window with it).
                let cx = if two { 1280 + 1196 } else { 1196 };
                session.input(&format!("click {cx} 52"));
                c.wait(Duration::from_secs(2), |_| false);
                continue;
            }
            if !to_chrome && std::env::var_os("HELIX_TEST_MENU").is_some() {
                // A menu open in Chrome (a popup with a grab) when the
                // overview opens.
                let cx = if two { 1920 } else { 640 };
                session.input(&format!("move {cx} 600"));
                session.input("button 273 1");
                c.wait(Duration::from_millis(600), |_| false);
                session.input("button 273 0");
                c.wait(Duration::from_millis(800), |_| false);
                let grabbed = session.log().lines().filter(|l| l.contains("popup")).count();
                eprintln!("round {round}: menu opened (popup log lines {grabbed})");
            }
            if std::env::var_os("HELIX_TEST_SUPER").is_some() {
                // As people open it: the Super key alone.
                session.input("key 125 1");
                c.wait(Duration::from_millis(80), |_| false);
                session.input("key 125 0");
            } else {
                overview(&session, true);
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let log = session.log();
                if log.lines().filter(|l| l.contains("overview tiles")).count() > drawn_before && all_tiles(&log).len() == 2 { break; }
                assert!(Instant::now() < deadline, "round {round}: no tiles:\n{log}");
                c.wait(Duration::from_millis(50), |_| false);
            }
            let dwell: u64 = std::env::var("HELIX_TEST_DWELL").ok().and_then(|v| v.parse().ok()).unwrap_or(1500);
            c.wait(Duration::from_millis(if to_chrome { dwell } else { 300 }), |_| false);
            let all = all_tiles(&session.log());
            let Some((x, y)) = all.iter().find(|(t, _)| t.starts_with(title)).map(|t| t.1) else { panic!("no tile {title}: {all:?}") };
            if to_chrome {
                let (w, _, a) = snap(&session, &mut c);
                let (_, _, b) = snap(&session, &mut c);
                let lx = if two { x - 1280 } else { x } as u32;
                eprintln!("round {round}: chrome tile live in overview: {}", rgb(&a, w, lx, (y - 40) as u32) != rgb(&b, w, lx, (y - 40) as u32));
            }
            session.input(&format!("move {x} {y}"));
            session.input("button 272 1");
            c.wait(Duration::from_millis(60), |_| false);
            session.input("button 272 0");
            c.wait(Duration::from_millis(if to_chrome { 800 } else { 300 }), |_| false);
            if !to_chrome {
                // Chrome hidden under Other for a while.
                let hidden: u64 = std::env::var("HELIX_TEST_HIDDEN").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
                c.wait(Duration::from_secs(hidden), |_| false);
                continue;
            }
            let (live, _) = probe(&session, &mut c, &format!("round {round} after activation"));
            session.input("tap 30");
            c.wait(Duration::from_millis(500), |_| false);
            let (_, key) = probe(&session, &mut c, "after key");
            session.input(if ozone == "x11" { "wheel 1" } else { "scroll 0 15" });
            c.wait(Duration::from_millis(500), |_| false);
            let (_, wheel) = probe(&session, &mut c, "after scroll");
            session.input("button 272 1\nbutton 272 0");
            c.wait(Duration::from_millis(500), |_| false);
            let (_, click) = probe(&session, &mut c, "after click");
            let outcome = format!("round {round}: drawing {live}, key {}, scroll {}, click {}", key == [0, 255, 0], wheel == [255, 0, 0], click == [0, 0, 255]);
            eprintln!("{outcome}");
            // Through Xwayland the page never sees the headless wheel (xev
            // does: overview_redraw_headless), before the overview too; so
            // it is not checked there.
            let wheel_ok = ozone == "x11" || wheel == [255, 0, 0];
            if !(live && key == [0, 255, 0] && wheel_ok && click == [0, 0, 255]) { failures.push(outcome); }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn chrome_wayland() { explore("wayland"); }

#[test]
fn chrome_x11() { explore("x11"); }

