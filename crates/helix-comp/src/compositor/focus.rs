//! What has keyboard focus: a Wayland surface, or an X11 window. X11
//! windows need the X input focus too (set when they get keyboard focus,
//! cleared when they lose it), which Smithay's `X11Surface` target does.

use std::borrow::Cow;

use smithay::backend::input::KeyState;
use smithay::desktop::PopupKind;
use smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
use smithay::input::Seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Serial};
use smithay::wayland::seat::WaylandFocus;
use smithay::xwayland::X11Surface;

use super::Helix;

#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardFocusTarget {
    Surface(WlSurface),
    X11(X11Surface),
}

impl IsAlive for KeyboardFocusTarget {
    fn alive(&self) -> bool {
        match self {
            Self::Surface(s) => s.alive(),
            Self::X11(x) => x.alive(),
        }
    }
}

impl WaylandFocus for KeyboardFocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Surface(s) => Some(Cow::Borrowed(s)),
            Self::X11(x) => x.wl_surface().map(Cow::Owned),
        }
    }
}

impl From<WlSurface> for KeyboardFocusTarget {
    fn from(surface: WlSurface) -> Self {
        Self::Surface(surface)
    }
}

/// Popup grabs hand their keyboard focus to the pointer: always a popup's
/// or a Wayland window's surface (an X11 window has none until Xwayland
/// associates it, and X11 apps don't use xdg popup grabs).
impl From<KeyboardFocusTarget> for WlSurface {
    fn from(focus: KeyboardFocusTarget) -> Self {
        match focus {
            KeyboardFocusTarget::Surface(s) => s,
            KeyboardFocusTarget::X11(x) => x.wl_surface().expect("an X11 window in a popup grab has a surface"),
        }
    }
}

impl From<PopupKind> for KeyboardFocusTarget {
    fn from(popup: PopupKind) -> Self {
        Self::Surface(popup.wl_surface().clone())
    }
}

impl KeyboardTarget<Helix> for KeyboardFocusTarget {
    fn enter(&self, seat: &Seat<Helix>, data: &mut Helix, keys: Vec<KeysymHandle<'_>>, serial: Serial) {
        match self {
            Self::Surface(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
            Self::X11(x) => KeyboardTarget::enter(x, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<Helix>, data: &mut Helix, serial: Serial) {
        match self {
            Self::Surface(s) => KeyboardTarget::leave(s, seat, data, serial),
            Self::X11(x) => KeyboardTarget::leave(x, seat, data, serial),
        }
    }

    fn key(&self, seat: &Seat<Helix>, data: &mut Helix, key: KeysymHandle<'_>, state: KeyState, serial: Serial, time: u32) {
        match self {
            Self::Surface(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
            Self::X11(x) => KeyboardTarget::key(x, seat, data, key, state, serial, time),
        }
    }

    fn modifiers(&self, seat: &Seat<Helix>, data: &mut Helix, modifiers: ModifiersState, serial: Serial) {
        match self {
            Self::Surface(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
            Self::X11(x) => KeyboardTarget::modifiers(x, seat, data, modifiers, serial),
        }
    }
}

/// What gets keyboard focus for `window`: its X11 window, or its surface.
pub fn focus_target(window: &smithay::desktop::Window) -> Option<KeyboardFocusTarget> {
    match window.x11_surface() {
        Some(x11) => Some(KeyboardFocusTarget::X11(x11.clone())),
        None => window.wl_surface().map(|s| KeyboardFocusTarget::Surface(s.into_owned())),
    }
}

impl Helix {
    /// The window a focus target belongs to.
    pub fn window_for_focus(&self, focus: &KeyboardFocusTarget) -> Option<smithay::desktop::Window> {
        match focus {
            KeyboardFocusTarget::X11(x11) => self
                .space
                .elements()
                .chain(self.minimized.iter())
                .chain(self.workspaces.parked())
                .find(|w| w.x11_surface() == Some(x11))
                .cloned(),
            KeyboardFocusTarget::Surface(surface) => self.window_for_surface(surface),
        }
    }

    /// The surface with keyboard focus (an X11 window's once it has one).
    pub fn focused_surface(&self) -> Option<WlSurface> {
        let focus = self.seat.get_keyboard()?.current_focus()?;
        focus.wl_surface().map(Cow::into_owned)
    }
}
