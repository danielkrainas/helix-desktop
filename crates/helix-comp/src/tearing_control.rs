//! `wp_tearing_control_v1`: surfaces asking to be presented without
//! waiting for vblank (helix-desktop-spec `contracts/wayland/README.md`,
//! "Tearing").
//!
//! Each surface's presentation hint is double-buffered state, applied by
//! its next commit ([`TearingCachedState`]); destroying its
//! `wp_tearing_control_v1` puts `vsync` back (also at the next commit).
//! Whether a frame really tears is decided per frame by [`may_tear`],
//! from facts the DRM backend gathers; other backends ignore the hint.

use std::sync::atomic::{AtomicBool, Ordering};

use smithay::reexports::wayland_protocols::wp::tearing_control::v1::server::wp_tearing_control_manager_v1::{self, WpTearingControlManagerV1};
use smithay::reexports::wayland_protocols::wp::tearing_control::v1::server::wp_tearing_control_v1::{self, WpTearingControlV1};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak};
use smithay::wayland::compositor::{with_states, Cacheable};

/// A surface's presentation hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hint {
    #[default]
    Vsync,
    /// Tearing is acceptable for lower latency.
    Async,
}

/// The hint, applied with the surface's next commit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TearingCachedState {
    pub hint: Hint,
}

impl Cacheable for TearingCachedState {
    fn commit(&mut self, _: &DisplayHandle) -> Self {
        *self
    }
    fn merge_into(self, into: &mut Self, _: &DisplayHandle) {
        *into = self;
    }
}

/// Whether a surface has a `wp_tearing_control_v1` (only one may exist).
#[derive(Debug, Default)]
struct HasControl(AtomicBool);

/// The surface's hint as last committed.
pub fn current_hint(surface: &WlSurface) -> Hint {
    with_states(surface, |states| states.cached_state.get::<TearingCachedState>().current().hint)
}

/// A `wp_tearing_control_v1`'s surface (inert once the surface is gone).
#[derive(Debug)]
pub struct ControlData {
    surface: Weak<WlSurface>,
}

/// Marker for delegating the protocol (`delegate_dispatch!` to it).
pub struct TearingControl;

/// What the protocol needs from the compositor.
pub trait TearingControlHandler:
    GlobalDispatch<WpTearingControlManagerV1, ()> + Dispatch<WpTearingControlManagerV1, ()> + Dispatch<WpTearingControlV1, ControlData> + 'static
{
}

impl<D> TearingControlHandler for D where
    D: GlobalDispatch<WpTearingControlManagerV1, ()> + Dispatch<WpTearingControlManagerV1, ()> + Dispatch<WpTearingControlV1, ControlData> + 'static
{
}

/// The manager global, version 1, for every client (Xwayland uses it for
/// X11 games presenting without vsync).
pub fn create_global<D: TearingControlHandler>(dh: &DisplayHandle) {
    dh.create_global::<D, WpTearingControlManagerV1, ()>(1, ());
}

impl<D: TearingControlHandler> GlobalDispatch<WpTearingControlManagerV1, (), D> for TearingControl {
    fn bind(_: &mut D, _: &DisplayHandle, _: &Client, resource: New<WpTearingControlManagerV1>, _: &(), init: &mut DataInit<'_, D>) {
        init.init(resource, ());
    }
}

impl<D: TearingControlHandler> Dispatch<WpTearingControlManagerV1, (), D> for TearingControl {
    fn request(_: &mut D, _: &Client, manager: &WpTearingControlManagerV1, request: wp_tearing_control_manager_v1::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, D>) {
        use wp_tearing_control_manager_v1::Request;
        match request {
            Request::GetTearingControl { id, surface } => {
                let taken = with_states(&surface, |states| {
                    states.data_map.insert_if_missing_threadsafe(HasControl::default);
                    states.data_map.get::<HasControl>().is_some_and(|h| h.0.swap(true, Ordering::SeqCst))
                });
                if taken {
                    // The new object stays uninitialized: the client is
                    // disconnected for the error.
                    manager.post_error(wp_tearing_control_manager_v1::Error::TearingControlExists, "the surface already has a wp_tearing_control_v1");
                    return;
                }
                init.init(id, ControlData { surface: surface.downgrade() });
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl<D: TearingControlHandler> Dispatch<WpTearingControlV1, ControlData, D> for TearingControl {
    fn request(_: &mut D, _: &Client, _: &WpTearingControlV1, request: wp_tearing_control_v1::Request, data: &ControlData, _: &DisplayHandle, _: &mut DataInit<'_, D>) {
        use wp_tearing_control_v1::{PresentationHint, Request};
        let Ok(surface) = data.surface.upgrade() else { return };
        match request {
            Request::SetPresentationHint { hint } => {
                let hint = match hint.into_result() {
                    Ok(PresentationHint::Async) => Hint::Async,
                    _ => Hint::Vsync,
                };
                with_states(&surface, |states| states.cached_state.get::<TearingCachedState>().pending().hint = hint);
            }
            Request::Destroy => {
                with_states(&surface, |states| {
                    states.cached_state.get::<TearingCachedState>().pending().hint = Hint::Vsync;
                    if let Some(has) = states.data_map.get::<HasControl>() {
                        has.0.store(false, Ordering::SeqCst);
                    }
                });
            }
            _ => {}
        }
    }
}

/// What decides whether one frame of a monitor may tear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Frame {
    /// The global setting (`org.helix.shell allow-tearing`).
    pub allowed: bool,
    /// The GPU can flip asynchronously (see [`async_flip_supported`]).
    pub supported: bool,
    /// The monitor is in full screen, its full-screen window is the
    /// topmost window there and has the keyboard.
    pub fullscreen_focused: bool,
    /// That window (a surface of it) asks for `async`.
    pub wants_async: bool,
    /// Something else is drawn over it: a layer surface on the top or
    /// overlay layer (the shell's menus, OSDs, overlays, the on-screen
    /// keyboard), the lock screen, the magnifier.
    pub covered: bool,
    /// The pointer image is shown on that monitor.
    pub cursor_shown: bool,
    /// A window or shell animation is running.
    pub animating: bool,
}

/// Whether the frame may be flipped asynchronously: only a focused
/// full-screen window that asks, alone on its monitor, with the setting
/// on and a GPU that can.
pub fn may_tear(f: Frame) -> bool {
    f.allowed && f.supported && f.fullscreen_focused && f.wants_async && !f.covered && !f.cursor_shown && !f.animating
}

/// The DRM capability that says a device can flip asynchronously: with
/// atomic modesetting `DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP` (Linux 6.8 and
/// later), else `DRM_CAP_ASYNC_PAGE_FLIP`.
pub fn async_flip_capability(atomic: bool) -> smithay::reexports::drm::DriverCapability {
    use smithay::reexports::drm::DriverCapability;
    if atomic {
        DriverCapability::AtomicASyncPageFlip
    } else {
        DriverCapability::ASyncPageFlip
    }
}

/// Whether `device` can flip asynchronously ([`async_flip_capability`]);
/// a capability it doesn't know counts as no.
pub fn async_flip_supported(device: &impl smithay::reexports::drm::Device, atomic: bool) -> bool {
    device.get_driver_capability(async_flip_capability(atomic)).is_ok_and(|v| v != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> Frame {
        Frame { allowed: true, supported: true, fullscreen_focused: true, wants_async: true, ..Frame::default() }
    }

    #[test]
    fn tears_only_when_everything_allows() {
        assert!(may_tear(ready()));
        assert!(!may_tear(Frame { allowed: false, ..ready() }), "the setting forbids it");
        assert!(!may_tear(Frame { supported: false, ..ready() }), "the GPU can't");
        assert!(!may_tear(Frame { fullscreen_focused: false, ..ready() }), "not a focused full-screen window");
        assert!(!may_tear(Frame { wants_async: false, ..ready() }), "it didn't ask");
        assert!(!may_tear(Frame { covered: true, ..ready() }), "a menu or overlay over it");
        assert!(!may_tear(Frame { cursor_shown: true, ..ready() }), "the pointer is shown");
        assert!(!may_tear(Frame { animating: true, ..ready() }), "an animation runs");
    }

    /// A DRM device that answers capability queries from a table.
    struct FakeDrm {
        caps: Vec<(u64, u64)>,
        file: std::fs::File,
    }

    impl std::os::fd::AsFd for FakeDrm {
        fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
            self.file.as_fd()
        }
    }

    impl smithay::reexports::drm::Device for FakeDrm {
        fn get_driver_capability(&self, cap: smithay::reexports::drm::DriverCapability) -> std::io::Result<u64> {
            self.caps.iter().find(|(c, _)| *c == cap as u64).map(|(_, v)| *v).ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))
        }
    }

    #[test]
    fn capability_check() {
        use smithay::reexports::drm::DriverCapability;
        // Never a real DRM node: a plain file stands in for the fd.
        let file = || std::fs::File::open("/dev/null").unwrap();
        let atomic_async = DriverCapability::AtomicASyncPageFlip as u64;
        let legacy_async = DriverCapability::ASyncPageFlip as u64;
        assert_eq!(atomic_async, 21);
        assert_eq!(legacy_async, 7);
        let both = FakeDrm { caps: vec![(atomic_async, 1), (legacy_async, 1)], file: file() };
        assert!(async_flip_supported(&both, true));
        assert!(async_flip_supported(&both, false));
        // A kernel before 6.8: legacy async flips only.
        let old = FakeDrm { caps: vec![(legacy_async, 1)], file: file() };
        assert!(!async_flip_supported(&old, true), "an unknown capability is no");
        assert!(async_flip_supported(&old, false));
        let none = FakeDrm { caps: vec![(atomic_async, 0), (legacy_async, 0)], file: file() };
        assert!(!async_flip_supported(&none, true));
        assert!(!async_flip_supported(&none, false));
    }
}
