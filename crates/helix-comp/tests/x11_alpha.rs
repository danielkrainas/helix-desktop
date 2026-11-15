//! X11 windows with an alpha channel keep it: the headless compositor with
//! Xwayland, and an X11 client (this test) mapping a 32-bit ARGB window
//! whose top-left corner is transparent, as GTK draws rounded client-side
//! decorations. The corner must show the desktop behind it, not black.
//!
//! Runs the real `helix-comp` binary headless, on a private runtime dir,
//! with no session bus (its D-Bus services just don't start). Skipped when
//! Xwayland is not installed.

use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ColormapAlloc, ConnectionExt, CreateGCAux, CreateWindowAux, ImageFormat, WindowClass};

/// The headless snapshot's backdrop where nothing is drawn.
const BACKDROP: u32 = 0x1E1E24;

struct Comp(Child);

impl Drop for Comp {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The snapshot's RGB at (x, y).
fn pixel(path: &Path, x: u32, y: u32) -> Option<u32> {
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).ok()?));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let i = ((y * info.width + x) * 4) as usize;
    Some(u32::from(buf[i]) << 16 | u32::from(buf[i + 1]) << 8 | u32::from(buf[i + 2]))
}

#[test]
fn argb_window_corners_stay_transparent() {
    if !Path::new("/usr/bin/Xwayland").exists() && !Path::new("/bin/Xwayland").exists() {
        eprintln!("no Xwayland; skipped");
        return;
    }
    let dir = std::env::temp_dir().join(format!("helix-x11-alpha-{}", std::process::id()));
    let runtime = dir.join("run");
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let snapshot = dir.join("snap.png");
    let display_file = dir.join("display");
    let command = dir.join("announce.sh");
    std::fs::write(&command, format!("#!/bin/sh\necho \"$DISPLAY\" > {}\n", display_file.display())).unwrap();
    std::fs::set_permissions(&command, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let comp = Command::new(env!("CARGO_BIN_EXE_helix-comp"))
        .args(["--backend", "headless", "--command"])
        .arg(&command)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("HELIX_HEADLESS_SIZE", "400x300")
        .env("HELIX_HEADLESS_SNAPSHOT", &snapshot)
        // Never the user's session bus.
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/helix-test")
        .env("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent/helix-test")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _comp = Comp(comp);
    let display: String = wait_for("Xwayland", || std::fs::read_to_string(&display_file).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()));

    let (conn, screen_num) = x11rb::connect(Some(&display)).unwrap();
    let screen = &conn.setup().roots[screen_num];
    let visual = screen
        .allowed_depths
        .iter()
        .filter(|d| d.depth == 32)
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.class == x11rb::protocol::xproto::VisualClass::TRUE_COLOR)
        .expect("a 32-bit visual")
        .visual_id;
    let colormap = conn.generate_id().unwrap();
    conn.create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual).unwrap();
    let (w, h) = (200u16, 100u16);
    let window = conn.generate_id().unwrap();
    conn.create_window(32, window, screen.root, 0, 0, w, h, 0, WindowClass::INPUT_OUTPUT, visual, &CreateWindowAux::new().border_pixel(0).background_pixel(0).colormap(colormap))
        .unwrap();
    conn.map_window(window).unwrap();
    // Opaque white, with a transparent 20×20 top-left corner.
    let mut pixels = Vec::with_capacity(usize::from(w) * usize::from(h) * 4);
    for y in 0..h {
        for x in 0..w {
            let argb: u32 = if x < 20 && y < 20 { 0 } else { 0xFFFF_FFFF };
            pixels.extend_from_slice(&argb.to_le_bytes());
        }
    }
    let gc = conn.generate_id().unwrap();
    conn.create_gc(gc, window, &CreateGCAux::new()).unwrap();
    let draw = |conn: &x11rb::rust_connection::RustConnection| {
        conn.put_image(ImageFormat::Z_PIXMAP, window, gc, w, h, 0, 0, 0, 32, &pixels).unwrap();
        conn.flush().unwrap();
    };
    draw(&conn);

    // Where the window lands (centered or not), from the server.
    let origin = |conn: &x11rb::rust_connection::RustConnection| {
        let t = conn.translate_coordinates(window, screen.root, 0, 0).ok()?.reply().ok()?;
        Some((t.dst_x.max(0) as u32, t.dst_y.max(0) as u32))
    };
    let (corner, inside) = wait_for("the window in a snapshot", || {
        draw(&conn);
        let (x, y) = origin(&conn)?;
        let inside = pixel(&snapshot, x + 100, y + 50)?;
        (inside == 0xFFFFFF).then(|| (pixel(&snapshot, x + 5, y + 5), inside))
    });
    assert_eq!(inside, 0xFFFFFF);
    assert_eq!(corner, Some(BACKDROP), "the transparent corner shows what is behind the window");
    let _ = std::fs::remove_dir_all(&dir);
}
