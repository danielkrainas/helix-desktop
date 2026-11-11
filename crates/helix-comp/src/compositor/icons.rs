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

/// What a window's icon is: its name and image buffers (by object).
pub type IconKey = (String, Vec<(ObjectId, i32)>);

/// The icon `window`'s app set: (name, buffers with their scale).
fn icon_of(window: &Window) -> (String, Vec<(WlBuffer, i32)>) {
    let Some(toplevel) = window.toplevel() else { return Default::default() };
    with_states(toplevel.wl_surface(), |states| {
        let mut cached = states.cached_state.get::<ToplevelIconCachedState>();
        let current = cached.current();
        (current.icon_name().unwrap_or_default().to_owned(), current.buffers().to_vec())
    })
}

pub fn icon_key(window: &Window) -> IconKey {
    let (name, buffers) = icon_of(window);
    (name, buffers.iter().map(|(b, s)| (b.id(), *s)).collect())
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

/// Tell `listener` window `id`'s icon.
pub fn send_icon(listener: &HelixWindowListV1, id: u32, window: &Window) {
    if listener.version() < 7 {
        return;
    }
    let (name, buffers) = icon_of(window);
    let (pixels, size) = largest_image(&buffers).unwrap_or_default();
    let Some(fd) = memfd(&pixels) else { return };
    listener.icon(id, name, size, fd.as_fd());
}

impl XdgToplevelIconHandler for Helix {}

smithay::delegate_xdg_toplevel_icon!(Helix);
