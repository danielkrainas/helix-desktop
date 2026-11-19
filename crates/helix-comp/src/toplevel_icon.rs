//! `xdg_toplevel_icon_manager_v1`: icons apps give their windows
//! (helixdesktop/spec `contracts/wayland/README.md`, "Window icons").
//!
//! An icon's pixels are copied when `add_buffer` arrives (the protocol
//! forbids changing them afterwards), so the buffer's lifetime does not
//! matter and `no_buffer` is never raised. Smithay's version checks that
//! lifetime from a hook which, when a client destroys the icon and then
//! its buffer in one batch (as Chromium-based apps do), fires after the
//! icon is gone and disconnects the client.
//!
//! What a client can make the compositor hold is limited: images larger
//! than [`MAX_SIZE`] pixels, past [`MAX_IMAGES`] in one icon, or past
//! [`CLIENT_BYTES`] of pixels per client are left out.

use std::sync::{Arc, Mutex};

use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel;
use smithay::reexports::wayland_protocols::xdg::toplevel_icon::v1::server::xdg_toplevel_icon_manager_v1::{self, XdgToplevelIconManagerV1};
use smithay::reexports::wayland_protocols::xdg::toplevel_icon::v1::server::xdg_toplevel_icon_v1::{self, XdgToplevelIconV1};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm::Format;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};
use smithay::wayland::compositor::{with_states, Cacheable};
use smithay::wayland::shm::{with_buffer_contents, BufferAccessError};

use crate::client_budget::{Budgets, Charge};

/// The largest image taken, in pixels a side: well above the largest
/// Helix asks for (96 at scale 4 is 384).
pub const MAX_SIZE: u32 = 512;
/// Images one icon may hold.
pub const MAX_IMAGES: usize = 8;
/// Icon pixels one client's icons may hold in all (live icons and those
/// given to its windows).
pub const CLIENT_BYTES: usize = 32 * 1024 * 1024;
static BUDGETS: Budgets = Budgets::new(CLIENT_BYTES);

/// One image of an icon: square, premultiplied ARGB8888 (native order).
#[derive(Debug, Clone)]
pub struct IconImage {
    pub size: u32,
    pub scale: i32,
    pub pixels: Arc<Vec<u8>>,
    /// Its bytes, counted against the client's [`CLIENT_BYTES`].
    _charge: Arc<Charge>,
}

/// An icon as a client built it.
#[derive(Debug, Clone, Default)]
pub struct Icon {
    pub name: Option<String>,
    pub images: Vec<IconImage>,
    /// Tells icons apart (for noticing a change).
    pub serial: u64,
}

/// A toplevel's icon, applied with the surface's next commit.
#[derive(Debug, Clone, Default)]
pub struct IconCachedState {
    pub icon: Option<Arc<Icon>>,
}

impl Cacheable for IconCachedState {
    fn commit(&mut self, _: &DisplayHandle) -> Self {
        self.clone()
    }
    fn merge_into(self, into: &mut Self, _: &DisplayHandle) {
        *into = self;
    }
}

/// An `xdg_toplevel_icon_v1` being built; `set` once given to a toplevel
/// (it may not change after that).
#[derive(Debug, Default)]
pub struct IconBuilder {
    icon: Mutex<Icon>,
    set: std::sync::atomic::AtomicBool,
}

pub(crate) static NEXT_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Why an icon buffer is refused.
#[derive(Debug, PartialEq, Eq)]
pub enum BufferProblem {
    /// Not wl_shm, or not square (`invalid_buffer`).
    Invalid(&'static str),
    /// Fine by the protocol, but in a format Helix does not read: the
    /// image is left out.
    Unreadable,
    /// Larger than [`MAX_SIZE`]: left out.
    TooLarge,
}

/// Copy a square wl_shm buffer's image as premultiplied ARGB8888
/// (native order), with its size.
fn copy_image(buffer: &WlBuffer) -> Result<(Vec<u8>, u32), BufferProblem> {
    let copied = with_buffer_contents(buffer, |ptr, len, data| {
        let (w, h, stride, offset) = (data.width as usize, data.height as usize, data.stride as usize, data.offset as usize);
        if w != h || w == 0 {
            return Err(BufferProblem::Invalid("The wl_buffer must be a square"));
        }
        if w > MAX_SIZE as usize {
            return Err(BufferProblem::TooLarge);
        }
        if offset + stride * h > len || stride < w * 4 {
            return Err(BufferProblem::Unreadable);
        }
        let opaque = data.format == Format::Xrgb8888;
        if !opaque && data.format != Format::Argb8888 {
            return Err(BufferProblem::Unreadable);
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
        Ok((out, w as u32))
    });
    match copied {
        Ok(result) => result,
        Err(BufferAccessError::NotManaged) => Err(BufferProblem::Invalid("The wl_buffer must be backed by wl_shm")),
        Err(_) => Err(BufferProblem::Unreadable),
    }
}

/// Marker for delegating the protocol (`delegate_dispatch!` to it).
pub struct ToplevelIcons;

/// What the protocol needs from the compositor.
pub trait ToplevelIconHandler:
    GlobalDispatch<XdgToplevelIconManagerV1, Vec<i32>> + Dispatch<XdgToplevelIconManagerV1, ()> + Dispatch<XdgToplevelIconV1, IconBuilder> + 'static
{
    /// The surface of `toplevel`, if it is one of the compositor's.
    fn toplevel_surface(&self, toplevel: &XdgToplevel) -> Option<WlSurface>;
}

/// The manager global, version 1; it tells clients the image `sizes`
/// the compositor wants.
pub fn create_global<D: ToplevelIconHandler>(dh: &DisplayHandle, sizes: &[i32]) {
    dh.create_global::<D, XdgToplevelIconManagerV1, Vec<i32>>(1, sizes.to_vec());
}

impl<D: ToplevelIconHandler> GlobalDispatch<XdgToplevelIconManagerV1, Vec<i32>, D> for ToplevelIcons {
    fn bind(_: &mut D, _: &DisplayHandle, _: &Client, resource: New<XdgToplevelIconManagerV1>, sizes: &Vec<i32>, init: &mut DataInit<'_, D>) {
        let manager = init.init(resource, ());
        for size in sizes {
            manager.icon_size(*size);
        }
        manager.done();
    }
}

impl<D: ToplevelIconHandler> Dispatch<XdgToplevelIconManagerV1, (), D> for ToplevelIcons {
    fn request(state: &mut D, _: &Client, _: &XdgToplevelIconManagerV1, request: xdg_toplevel_icon_manager_v1::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, D>) {
        use xdg_toplevel_icon_manager_v1::Request;
        match request {
            Request::CreateIcon { id } => {
                init.init(id, IconBuilder::default());
            }
            Request::SetIcon { toplevel, icon } => {
                let Some(surface) = state.toplevel_surface(&toplevel) else { return };
                let icon = icon.and_then(|icon| {
                    let builder = icon.data::<IconBuilder>()?;
                    builder.set.store(true, std::sync::atomic::Ordering::Relaxed);
                    let built = builder.icon.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    // An icon with neither name nor images unsets it.
                    (built.name.is_some() || !built.images.is_empty()).then(|| Arc::new(Icon { serial: NEXT_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed), ..built }))
                });
                with_states(&surface, |states| states.cached_state.get::<IconCachedState>().pending().icon = icon);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl<D: ToplevelIconHandler> Dispatch<XdgToplevelIconV1, IconBuilder, D> for ToplevelIcons {
    fn request(_: &mut D, client: &Client, icon: &XdgToplevelIconV1, request: xdg_toplevel_icon_v1::Request, data: &IconBuilder, _: &DisplayHandle, _: &mut DataInit<'_, D>) {
        use xdg_toplevel_icon_v1::Request;
        let immutable = || icon.post_error(xdg_toplevel_icon_v1::Error::Immutable, "the icon was already given to a toplevel with set_icon");
        match request {
            Request::SetName { icon_name } => {
                if data.set.load(std::sync::atomic::Ordering::Relaxed) {
                    return immutable();
                }
                data.icon.lock().unwrap_or_else(|e| e.into_inner()).name = Some(icon_name);
            }
            Request::AddBuffer { buffer, scale } => {
                if data.set.load(std::sync::atomic::Ordering::Relaxed) {
                    return immutable();
                }
                match copy_image(&buffer) {
                    Ok((pixels, size)) => {
                        let mut built = data.icon.lock().unwrap_or_else(|e| e.into_inner());
                        // A later image of the same size and scale replaces the earlier.
                        built.images.retain(|m| !(m.size == size && m.scale == scale));
                        if built.images.len() >= MAX_IMAGES {
                            tracing::debug!("icon has {MAX_IMAGES} images already; left out");
                            return;
                        }
                        let Some(charge) = BUDGETS.charge(client.id(), pixels.len()) else {
                            tracing::debug!("client's icons hold too many pixels; image left out");
                            return;
                        };
                        built.images.push(IconImage { size, scale, pixels: Arc::new(pixels), _charge: Arc::new(charge) });
                    }
                    Err(BufferProblem::Invalid(message)) => icon.post_error(xdg_toplevel_icon_v1::Error::InvalidBuffer, message),
                    Err(BufferProblem::Unreadable) => tracing::debug!("icon buffer in a format Helix does not read; left out"),
                    Err(BufferProblem::TooLarge) => tracing::debug!("icon image larger than {MAX_SIZE} pixels; left out"),
                }
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

