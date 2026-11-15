//! A full-screen test window: an xdg toplevel that asks to be full screen
//! and paints a red and green checker in shared memory (which the
//! headless backend's snapshots show). For checking what the shell shows
//! over full-screen windows, e.g. the OSD:
//!
//! ```text
//! HELIX_HEADLESS_SNAPSHOT=shot.png helix-comp --backend headless \
//!     --shell target/debug/helix-shell --command target/debug/examples/fullscreen
//! gdbus call --session --dest org.gnome.Shell --object-path /org/gnome/Shell \
//!     --method org.gnome.Shell.ShowOSD "{'icon': <'audio-volume-medium-symbolic'>, 'level': <0.45>}"
//! ```
//!
//! (on a private session bus, with a scratch HOME and XDG directories).
use std::os::fd::AsFd;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
struct S {
    size: (i32, i32),
    serial: Option<u32>,
}
macro_rules! quiet {
    ($($i:ty),*) => {$(impl Dispatch<$i, ()> for S { fn event(_: &mut Self, _: &$i, _: <$i as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {} })*};
}
quiet!(wl_compositor::WlCompositor, wl_surface::WlSurface, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer);
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for S {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for S {
    fn event(_: &mut Self, b: &xdg_wm_base::XdgWmBase, e: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = e {
            b.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for S {
    fn event(s: &mut Self, _: &xdg_surface::XdgSurface, e: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = e {
            s.serial = Some(serial);
        }
    }
}
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for S {
    fn event(s: &mut Self, _: &xdg_toplevel::XdgToplevel, e: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_toplevel::Event::Configure { width, height, .. } = e {
            if width > 0 && height > 0 {
                s.size = (width, height);
            }
        }
    }
}

fn main() {
    let conn = Connection::connect_to_env().unwrap();
    let (globals, mut queue) = registry_queue_init::<S>(&conn).unwrap();
    let qh = queue.handle();
    let comp: wl_compositor::WlCompositor = globals.bind(&qh, 4..=4, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=3, ()).unwrap();
    let surface = comp.create_surface(&qh, ());
    let xs = wm.get_xdg_surface(&surface, &qh, ());
    let top = xs.get_toplevel(&qh, ());
    top.set_title("Full screen".into());
    top.set_app_id("helix-fullscreen-test".into());
    top.set_fullscreen(None);
    surface.commit();
    let mut s = S::default();
    let mut drawn = (0, 0);
    loop {
        queue.blocking_dispatch(&mut s).unwrap();
        if let Some(serial) = s.serial.take() {
            xs.ack_configure(serial);
            let (w, h) = if s.size.0 > 0 { s.size } else { (1280, 720) };
            if (w, h) != drawn {
                drawn = (w, h);
                let file = shm_file();
                let mut px = vec![0u8; w as usize * h as usize * 4];
                for y in 0..h as usize {
                    for x in 0..w as usize {
                        let c: [u8; 4] = if (x / 80 + y / 80) % 2 == 0 { [0x20, 0x20, 0xC0, 0xFF] } else { [0x20, 0xA0, 0x20, 0xFF] };
                        px[(y * w as usize + x) * 4..][..4].copy_from_slice(&c);
                    }
                }
                use std::io::Write;
                (&file).write_all(&px).unwrap();
                let pool = shm.create_pool(file.as_fd(), w * h * 4, &qh, ());
                let buf = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, &qh, ());
                surface.attach(Some(&buf), 0, 0);
                surface.damage_buffer(0, 0, w, h);
            }
            surface.commit();
        }
    }
}

/// An unlinked file in the runtime dir for the pixels.
fn shm_file() -> std::fs::File {
    let dir = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR");
    let path = format!("{dir}/helix-fullscreen-{}", std::process::id());
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).expect("a file for the pixels");
    let _ = std::fs::remove_file(&path);
    file
}
