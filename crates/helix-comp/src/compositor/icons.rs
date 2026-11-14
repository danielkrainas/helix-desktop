//! Window icons apps set (`xdg_toplevel_icon_manager_v1`), passed on to the
//! shell's window list (helix-desktop-spec `contracts/wayland/README.md`,
//! "Window icons" and "Window switching").
//! The protocol itself is `helix_comp::toplevel_icon`.

use std::io::Write;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::Arc;

use helix_comp::toplevel_icon::{Icon, IconBuilder, IconCachedState, ToplevelIconHandler, ToplevelIcons};
use helix_protocols::server::helix_window_list_v1::HelixWindowListV1;
use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel;
use smithay::reexports::wayland_protocols::xdg::toplevel_icon::v1::server::xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1;
use smithay::reexports::wayland_protocols::xdg::toplevel_icon::v1::server::xdg_toplevel_icon_v1::XdgToplevelIconV1;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{delegate_dispatch, delegate_global_dispatch, Resource};
use smithay::wayland::compositor::with_states;

use super::Helix;

/// Image sizes Helix asks apps for.
pub const ICON_SIZES: [i32; 3] = [24, 48, 96];

/// What a window's icon is: its name, which icon it is (a number given to
/// each icon a client builds), and for X11 windows how many times its
/// `_NET_WM_ICON` was read.
pub type IconKey = (String, u64, u64);

/// X11 window icons, by X window id: (times read, icon).
#[derive(Default)]
pub struct IconState {
    x11: std::collections::HashMap<u32, (u64, Option<super::x11ping::X11Icon>)>,
}

/// The icon `window`'s app set.
fn icon_of(window: &Window) -> Option<Arc<Icon>> {
    let toplevel = window.toplevel()?;
    with_states(toplevel.wl_surface(), |states| states.cached_state.get::<IconCachedState>().current().icon.clone())
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
                Some((generation, Some(_))) => (String::new(), 0, *generation),
                _ => IconKey::default(),
            };
        }
        match icon_of(window) {
            Some(icon) => (icon.name.clone().unwrap_or_default(), icon.serial, 0),
            None => IconKey::default(),
        }
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
                let icon = icon_of(window);
                let name = icon.as_ref().and_then(|i| i.name.clone()).unwrap_or_default();
                let largest = icon.as_ref().and_then(|i| i.images.iter().max_by_key(|m| m.size)).map(|m| (m.pixels.to_vec(), m.size));
                (name, largest.unwrap_or_default())
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

impl ToplevelIconHandler for Helix {
    fn toplevel_surface(&self, toplevel: &XdgToplevel) -> Option<WlSurface> {
        self.xdg_shell_state.get_toplevel(toplevel).map(|t| t.wl_surface().clone())
    }
}

delegate_global_dispatch!(Helix: [XdgToplevelIconManagerV1: Vec<i32>] => ToplevelIcons);
delegate_dispatch!(Helix: [XdgToplevelIconManagerV1: ()] => ToplevelIcons);
delegate_dispatch!(Helix: [XdgToplevelIconV1: IconBuilder] => ToplevelIcons);
