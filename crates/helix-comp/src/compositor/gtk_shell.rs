//! `gtk_shell1`, GNOME's own protocol, in the part GTK uses with Helix
//! (helixdesktop/spec `contracts/wayland/README.md`, "GTK shell").
//!
//! GTK 3 has no other way to say a dialog is modal
//! (`gtk_surface1.set_modal`; GTK 4 uses `xdg_dialog_v1` when it is
//! there), and Helix needs that to send focus meant for a window to its
//! modal dialog, as mutter does. Version 7: title bar gestures
//! (`titlebar_gesture`, version 5: GTK then leaves double, middle and
//! right clicks on its own title bars to the compositor, which does what
//! `action-*-click-titlebar` say, as on Helix's title bars), surface
//! offsets (version 6, `wl_surface.offset`) and the accessibility names
//! (`set_a11y_properties`, version 7) that
//! `org.freedesktop.a11y.PointerLocator` reports for a window. Nothing
//! else changes for GTK while `xdg_activation_v1` is offered (GTK then
//! activates with tokens, not `present` or `request_focus`).

use std::sync::atomic::{AtomicBool, Ordering};

use helix_protocols::gtk_shell_server::{gtk_shell1, gtk_surface1};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{backend::ClientId, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak};
use smithay::wayland::xdg_activation::XdgActivationToken;

use super::Helix;

/// The version offered (see the module documentation).
pub const VERSION: u32 = 7;

/// A surface's accessibility names (`set_a11y_properties`): its app's
/// name on the accessibility bus and its toplevel's object path.
#[derive(Debug, Clone, Default)]
pub struct A11yProperties(pub Option<(String, String)>);

/// The accessibility names `surface`'s client gave it, if any.
pub fn a11y_properties(surface: &WlSurface) -> Option<(String, String)> {
    smithay::wayland::compositor::with_states(surface, |states| {
        states.data_map.get::<std::sync::Mutex<A11yProperties>>().and_then(|p| p.lock().ok().and_then(|p| p.0.clone()))
    })
}

/// A `gtk_surface1`'s surface.
pub struct GtkSurfaceData {
    surface: Weak<WlSurface>,
    /// Whether this object made it modal.
    modal: AtomicBool,
}

pub fn create_global(dh: &DisplayHandle) {
    dh.create_global::<Helix, gtk_shell1::GtkShell1, ()>(VERSION, ());
}

impl GlobalDispatch<gtk_shell1::GtkShell1, ()> for Helix {
    fn bind(_: &mut Self, _: &DisplayHandle, _: &Client, resource: New<gtk_shell1::GtkShell1>, _: &(), init: &mut DataInit<'_, Self>) {
        // No global app menu, menu bar or desktop icons.
        init.init(resource, ()).capabilities(0);
    }
}

impl Dispatch<gtk_shell1::GtkShell1, ()> for Helix {
    fn request(state: &mut Self, _: &Client, _: &gtk_shell1::GtkShell1, request: gtk_shell1::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        match request {
            gtk_shell1::Request::GetGtkSurface { gtk_surface, surface } => {
                init.init(gtk_surface, GtkSurfaceData { surface: surface.downgrade(), modal: AtomicBool::new(false) });
            }
            gtk_shell1::Request::SystemBell { surface } => {
                let surface = surface.and_then(|s| s.data::<GtkSurfaceData>().and_then(|d| d.surface.upgrade().ok()));
                state.ring_bell(surface.as_ref());
            }
            // Startup notification is done with xdg_activation_v1 tokens.
            gtk_shell1::Request::SetStartupId { .. } | gtk_shell1::Request::NotifyLaunch { .. } => {}
            _ => {}
        }
    }
}

impl Dispatch<gtk_surface1::GtkSurface1, GtkSurfaceData> for Helix {
    fn request(state: &mut Self, _: &Client, _: &gtk_surface1::GtkSurface1, request: gtk_surface1::Request, data: &GtkSurfaceData, _: &DisplayHandle, _: &mut DataInit<'_, Self>) {
        let Ok(surface) = data.surface.upgrade() else { return };
        match request {
            gtk_surface1::Request::SetModal | gtk_surface1::Request::UnsetModal => {
                let modal = matches!(request, gtk_surface1::Request::SetModal);
                data.modal.store(modal, Ordering::Relaxed);
                state.set_modal(&surface, modal);
            }
            // As an activation without a token: the window wants
            // attention instead of taking focus.
            gtk_surface1::Request::Present { .. } => {
                if let Some(window) = state.window_for_surface(&surface) {
                    state.set_attention(&window, true);
                }
            }
            // The startup id is an activation token (GTK passes on the
            // XDG_ACTIVATION_TOKEN it was started with).
            gtk_surface1::Request::RequestFocus { startup_id } => {
                let token = startup_id.map(XdgActivationToken::from);
                state.activate_with_token(token.as_ref(), &surface);
            }
            gtk_surface1::Request::TitlebarGesture { gesture, .. } => {
                let gesture = match gesture {
                    smithay::reexports::wayland_server::WEnum::Value(g) => g as u32,
                    smithay::reexports::wayland_server::WEnum::Unknown(g) => g,
                };
                if let Some(window) = state.window_for_surface(&surface) {
                    state.titlebar_gesture(&window, gesture);
                }
            }
            gtk_surface1::Request::SetA11yProperties { a11y_dbus_name, toplevel_object_path } => {
                smithay::wayland::compositor::with_states(&surface, |states| {
                    states.data_map.insert_if_missing_threadsafe(|| std::sync::Mutex::new(A11yProperties::default()));
                    if let Some(Ok(mut p)) = states.data_map.get::<std::sync::Mutex<A11yProperties>>().map(|p| p.lock()) {
                        p.0 = Some((a11y_dbus_name, toplevel_object_path));
                    }
                });
            }
            // App menu paths: nothing uses them.
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _: ClientId, _: &gtk_surface1::GtkSurface1, data: &GtkSurfaceData) {
        // Modal was that object's state.
        if let (true, Ok(surface)) = (data.modal.load(Ordering::Relaxed), data.surface.upgrade()) {
            state.set_modal(&surface, false);
        }
    }
}
