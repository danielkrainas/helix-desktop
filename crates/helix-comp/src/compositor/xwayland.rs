//! Xwayland: runs X11 apps as clients of Helix.
//!
//! The compositor starts an Xwayland server and acts as its X11 window
//! manager (Smithay's `X11Wm`). X11 windows join the same space as Wayland
//! windows and share focus, stacking, move/resize, maximize/fullscreen and
//! the clipboard. Integration follows Smithay's `anvil` example (MIT).

use std::os::fd::OwnedFd;
use std::process::Stdio;

use smithay::desktop::Window;
use smithay::input::pointer::Focus;
use smithay::utils::{Logical, Rectangle, Size, SERIAL_COUNTER};
use smithay::wayland::selection::data_device::{
    clear_data_device_selection, current_data_device_selection_userdata,
    request_data_device_client_selection, set_data_device_selection,
};
use smithay::wayland::selection::primary_selection::{
    clear_primary_selection, current_primary_selection_userdata, request_primary_client_selection,
    set_primary_selection,
};
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{Reorder, ResizeEdge as X11ResizeEdge, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler};

use super::grabs::{Edges, MoveGrab, ResizeGrab, ResizeTracking};
use super::Helix;

impl Helix {
    /// Start Xwayland. `ready` runs once it is up (with its display number)
    /// or has failed (`None`).
    pub fn start_xwayland(&mut self, ready: impl FnOnce(&mut Helix, Option<u32>) + 'static) {
        let spawned = XWayland::spawn(
            &self.display_handle,
            None,
            std::iter::empty::<(String, String)>(),
            true,
            Stdio::null(),
            Stdio::null(),
            |_| (),
        );
        let (xwayland, client) = match spawned {
            Ok(x) => x,
            Err(err) => {
                tracing::warn!(%err, "cannot start Xwayland; X11 apps will not run");
                ready(self, None);
                return;
            }
        };
        let mut ready = Some(ready);
        let result = self.loop_handle.insert_source(xwayland, move |event, _, state| {
            let display = match event {
                XWaylandEvent::Ready { x11_socket, display_number } => {
                    match X11Wm::start_wm(state.loop_handle.clone(), x11_socket, client.clone()) {
                        Ok(mut wm) => {
                            let (pixels, w, h) = super::cursor::arrow();
                            // The arrow is BGRA; X wants RGBA.
                            let rgba: Vec<u8> = pixels
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .flat_map(|p| [p[2], p[1], p[0], p[3]])
                                .collect();
                            if let Err(err) = wm.set_cursor(&rgba, Size::from((w as u16, h as u16)), (0u16, 0u16).into()) {
                                tracing::warn!(%err, "cannot set the X11 cursor");
                            }
                            state.xwm = Some(wm);
                            state.xdisplay = Some(display_number);
                            let (answers, received) = smithay::reexports::calloop::channel::channel::<u32>();
                            match super::x11ping::X11Ping::start(display_number, answers) {
                                Ok(ping) => {
                                    state.x11ping = Some(ping);
                                    let _ = state.loop_handle.insert_source(received, |event, _, state| {
                                        if let smithay::reexports::calloop::channel::Event::Msg(id) = event {
                                            state.x11_pong(id);
                                        }
                                    });
                                }
                                Err(err) => tracing::warn!(%err, "cannot check X11 apps are responding"),
                            }
                            tracing::info!(display = display_number, "Xwayland ready");
                            Some(display_number)
                        }
                        Err(err) => {
                            tracing::warn!(%err, "cannot manage Xwayland windows");
                            None
                        }
                    }
                }
                XWaylandEvent::Error => {
                    tracing::warn!("Xwayland failed to start; X11 apps will not run");
                    None
                }
            };
            if let Some(ready) = ready.take() {
                ready(state, display);
            }
        });
        if let Err(err) = result {
            tracing::warn!(%err, "cannot watch Xwayland");
        }
    }

    fn x11_window(&self, surface: &X11Surface) -> Option<Window> {
        self.space
            .elements()
            .chain(self.minimized.iter())
            .find(|w| w.x11_surface() == Some(surface))
            .cloned()
    }
}

impl XWaylandShellHandler for Helix {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    /// The window got its surface: if it was focused when it was mapped
    /// (before it had one), give it the keyboard (properly) now.
    fn surface_associated(&mut self, _xwm: XwmId, wl_surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface, surface: X11Surface) {
        if surface.is_override_redirect() || !surface.is_activated() || self.lock.locked {
            return;
        }
        // It got focus while it had no surface: enter again, now with it
        // (the X input focus stays on it).
        let _ = wl_surface;
        let target = super::focus::KeyboardFocusTarget::X11(surface);
        let keyboard = self.seat.get_keyboard().unwrap();
        let focus = keyboard.current_focus();
        if focus.is_none() || focus.as_ref() == Some(&target) {
            keyboard.set_focus(self, Option::<super::focus::KeyboardFocusTarget>::None, SERIAL_COUNTER.next_serial());
            keyboard.set_focus(self, Some(target), SERIAL_COUNTER.next_serial());
        }
    }
}

fn edges(edge: X11ResizeEdge) -> Edges {
    use X11ResizeEdge::*;
    // xdg_toplevel resize edge bits: top 1, bottom 2, left 4, right 8.
    let bits = match edge {
        Top => 1,
        Bottom => 2,
        Left => 4,
        TopLeft => 5,
        BottomLeft => 6,
        Right => 8,
        TopRight => 9,
        BottomRight => 10,
    };
    Edges::from_bits(bits)
}

impl XwmHandler for Helix {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("X11 WM events only arrive while it runs")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Err(err) = surface.set_mapped(true) {
            tracing::warn!(%err, "cannot map X11 window");
            return;
        }
        let window = Window::new_x11_window(surface.clone());
        let (x, y) = self.next_window_position();
        // Room for the title bar Helix draws above it.
        let location: smithay::utils::Point<i32, Logical> = (x, y + super::frames::frame_top(&window)).into();
        let size = surface.geometry().size;
        let _ = surface.configure(Rectangle::new(location, size));
        tracing::info!(title = surface.title(), class = surface.class(), ?location, ?size, "new X11 window");
        self.map_new_window(&window, location);
        self.place_new_window(&window);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        let location = surface.geometry().loc;
        self.space.map_element(Window::new_x11_window(surface), location, false);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.forget_window(&window);
        }
        if !surface.is_override_redirect() {
            let _ = surface.set_mapped(false);
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.forget_window(&window);
        }
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // Apps may resize themselves; Helix keeps deciding where they go.
        let mut geo = surface.geometry();
        if let Some(w) = w {
            geo.size.w = w as i32;
        }
        if let Some(h) = h {
            geo.size.h = h as i32;
        }
        let _ = surface.configure(geo);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        tracing::debug!(?geometry, or = surface.is_override_redirect(), "X11 configure notify");
        // Override-redirect windows place themselves; follow them.
        if let Some(window) = self.x11_window(&surface) {
            if surface.is_override_redirect() {
                self.space.map_element(window, geometry.loc, false);
            }
        }
    }

    fn maximize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.maximize(&window);
        }
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.restore_window(&window);
        }
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.fullscreen(&window, None);
        }
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.restore_window(&window);
        }
    }

    fn minimize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.minimize(&window);
        }
    }

    fn move_request(&mut self, _xwm: XwmId, surface: X11Surface, _button: u32) {
        let Some(window) = self.x11_window(&surface) else { return };
        let pointer = self.seat.get_pointer().unwrap();
        let Some(start_data) = pointer.grab_start_data() else { return };
        self.restore_window(&window);
        let initial_location = self.space.element_location(&window).unwrap_or_default();
        pointer.set_grab(
            self,
            MoveGrab {
                start_data,
                window,
                initial_location,
                follow: false,
                unmaximize: false,
            },
            SERIAL_COUNTER.next_serial(),
            Focus::Clear,
        );
    }

    fn resize_request(&mut self, _xwm: XwmId, surface: X11Surface, _button: u32, edge: X11ResizeEdge) {
        let Some(window) = self.x11_window(&surface) else { return };
        let pointer = self.seat.get_pointer().unwrap();
        let Some(start_data) = pointer.grab_start_data() else { return };
        let location = self.space.element_location(&window).unwrap_or_default();
        let initial = Rectangle::new(location, window.geometry().size);
        let edges = edges(edge);
        self.resize = Some(ResizeTracking {
            window: window.clone(),
            edges,
            initial,
            finished: false,
        });
        pointer.set_grab(
            self,
            ResizeGrab {
                start_data,
                window,
                edges,
                initial,
                last_size: initial.size,
                follow: false,
            },
            SERIAL_COUNTER.next_serial(),
            Focus::Clear,
        );
    }

    // ----- Clipboard and primary selection between X11 and Wayland -------

    fn allow_selection_access(&mut self, xwm: XwmId, _selection: SelectionTarget) -> bool {
        // Only while an X11 window of this Xwayland has keyboard focus.
        let focus = self.seat.get_keyboard().and_then(|k| k.current_focus());
        focus
            .and_then(|focus| self.window_for_focus(&focus))
            .and_then(|w| w.x11_surface().and_then(|x| x.xwm_id()))
            .is_some_and(|id| id == xwm)
    }

    fn send_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_type: String, fd: OwnedFd) {
        if selection == SelectionTarget::Clipboard {
            let current = current_data_device_selection_userdata(&self.seat).map(|r| r.clone());
            match current.as_ref() {
                Some(super::ServerSelection::Image(png)) => {
                    super::capture::serve_image(png.clone(), &mime_type, fd);
                    return;
                }
                Some(super::ServerSelection::Remote { session }) => {
                    let session = *session;
                    self.start_transfer(session, mime_type, fd);
                    return;
                }
                _ => {}
            }
        }
        let result = match selection {
            SelectionTarget::Clipboard => request_data_device_client_selection(&self.seat, mime_type, fd)
                .map_err(|e| e.to_string()),
            SelectionTarget::Primary => request_primary_client_selection(&self.seat, mime_type, fd)
                .map_err(|e| e.to_string()),
        };
        if let Err(err) = result {
            tracing::warn!(%err, ?selection, "cannot hand the Wayland selection to X11");
        }
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        tracing::debug!(?selection, ?mime_types, "X11 selection offered to Wayland clients");
        match selection {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.display_handle, &self.seat, mime_types.clone(), super::ServerSelection::X11);
                self.clipboard_changed(mime_types, None);
            }
            SelectionTarget::Primary => {
                set_primary_selection(&self.display_handle, &self.seat, mime_types, super::ServerSelection::X11)
            }
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        match selection {
            SelectionTarget::Clipboard => {
                let x11 = matches!(current_data_device_selection_userdata(&self.seat).as_deref(), Some(super::ServerSelection::X11));
                if x11 {
                    clear_data_device_selection(&self.display_handle, &self.seat);
                    self.clipboard_changed(Vec::new(), None);
                }
            }
            SelectionTarget::Primary => {
                let x11 = matches!(current_primary_selection_userdata(&self.seat).as_deref(), Some(super::ServerSelection::X11));
                if x11 {
                    clear_primary_selection(&self.display_handle, &self.seat);
                }
            }
        }
    }
}
