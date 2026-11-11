//! Window icons apps set (`xdg_toplevel_icon_manager_v1`), passed on to the
//! shell's window list (helix-desktop-spec `contracts/wayland/README.md`,
//! "Window switching").

use std::io::Write;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};

use helix_protocols::server::helix_window_list_v1::HelixWindowListV1;
use smithay::desktop::Window;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm::Format;
use smithay::reexports::wayland_server::Resource;
use smithay::wayland::compositor::with_states;
use smithay::wayland::shm::with_buffer_contents;
use smithay::wayland::xdg_toplevel_icon::{ToplevelIconCachedState, XdgToplevelIconHandler};

use super::Helix;

/// Image sizes Helix asks apps for.
pub const ICON_SIZES: [i32; 3] = [24, 48, 96];

/// What a window's icon is: its name, image buffers (by object), and for
/// X11 windows how many times its `_NET_WM_ICON` was read.
pub type IconKey = (String, Vec<(ObjectId, i32)>, u64);

/// X11 window icons, by X window id: (times read, icon).
#[derive(Default)]
pub struct IconState {
    x11: std::collections::HashMap<u32, (u64, Option<super::x11ping::X11Icon>)>,
}

/// The icon `window`'s app set: (name, buffers with their scale).
fn icon_of(window: &Window) -> (String, Vec<(WlBuffer, i32)>) {
    let Some(toplevel) = window.toplevel() else { return Default::default() };
    with_states(toplevel.wl_surface(), |states| {
        let mut cached = states.cached_state.get::<ToplevelIconCachedState>();
        let current = cached.current();
        (current.icon_name().unwrap_or_default().to_owned(), current.buffers().to_vec())
    })
}

impl Helix {
    /// What `window`'s icon is now (reading an X11 window's the first time).
    pub fn icon_key(&mut self, window: &Window) -> IconKey {
        if let Some(x11) = window.x11_surface() {
            let id = x11.window_id();
            if !self.icons.x11.contains_key(&id) {
                let icon = self.x11ping.as_ref().and_then(|p| p.watch_icon(id));
                self.icons.x11.insert(id, (1, icon));
            }
            return match self.icons.x11.get(&id) {
                Some((generation, Some(_))) => (String::new(), Vec::new(), *generation),
                _ => IconKey::default(),
            };
        }
        let (name, buffers) = icon_of(window);
        (name, buffers.iter().map(|(b, s)| (b.id(), *s)).collect(), 0)
    }

    /// An X11 window's `_NET_WM_ICON` changed.
    pub fn x11_icon_changed(&mut self, id: u32) {
        let icon = self.x11ping.as_ref().and_then(|p| p.icon(id));
        let entry = self.icons.x11.entry(id).or_insert((0, None));
        entry.0 += 1;
        entry.1 = icon;
    }

    /// Tell `listener` window `id`'s icon.
    pub fn send_icon(&self, listener: &HelixWindowListV1, id: u32, window: &Window) {
        if listener.version() < 7 {
            return;
        }
        let (name, (pixels, size)) = match window.x11_surface() {
            Some(x11) => (String::new(), self.icons.x11.get(&x11.window_id()).and_then(|(_, i)| i.clone()).unwrap_or_default()),
            None => {
                let (name, buffers) = icon_of(window);
                (name, largest_image(&buffers).unwrap_or_default())
            }
        };
        let Some(fd) = memfd(&pixels) else { return };
        listener.icon(id, name, size, fd.as_fd());
    }

    /// Forget icons of X11 windows that are gone.
    pub fn forget_x11_icons(&mut self, alive: &[u32]) {
        self.icons.x11.retain(|id, _| alive.contains(id));
    }
}

/// The largest image as premultiplied ARGB8888 (native order), and its size.
fn largest_image(buffers: &[(WlBuffer, i32)]) -> Option<(Vec<u8>, u32)> {
    let mut best: Option<(Vec<u8>, u32)> = None;
    for (buffer, _) in buffers {
        let image = with_buffer_contents(buffer, |ptr, len, data| {
            let (w, h, stride, offset) = (data.width as usize, data.height as usize, data.stride as usize, data.offset as usize);
            if w != h || w == 0 || offset + stride * h > len || stride < w * 4 {
                return None;
            }
            let opaque = data.format == Format::Xrgb8888;
            if !opaque && data.format != Format::Argb8888 {
                return None;
            }
            let mut out = Vec::with_capacity(w * h * 4);
            for row in 0..h {
                // SAFETY: inside the pool, checked against `len` above.
                let line = unsafe { std::slice::from_raw_parts(ptr.add(offset + row * stride), w * 4) };
                out.extend_from_slice(line);
            }
            if opaque {
                for px in out.as_chunks_mut::<4>().0 {
                    // Native little-endian ARGB: alpha is the last byte.
                    px[3] = 255;
                }
            }
            Some((out, w as u32))
        })
        .ok()
        .flatten();
        if let Some((pixels, size)) = image {
            if best.as_ref().is_none_or(|(_, s)| size > *s) {
                best = Some((pixels, size));
            }
        }
    }
    best
}

/// A sealed-size memfd holding `bytes`.
fn memfd(bytes: &[u8]) -> Option<OwnedFd> {
    // SAFETY: memfd_create returns a new fd we own (or -1).
    let raw = unsafe { libc::memfd_create(c"helix-icon".as_ptr(), libc::MFD_CLOEXEC) };
    if raw < 0 {
        return None;
    }
    // SAFETY: just created.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut file = std::fs::File::from(fd.try_clone().ok()?);
    file.write_all(bytes).ok()?;
    Some(fd)
}

impl XdgToplevelIconHandler for Helix {}

smithay::delegate_xdg_toplevel_icon!(Helix);
