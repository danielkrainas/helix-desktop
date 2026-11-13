//! Wayland protocol handlers.

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::desktop::{
    find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output, LayerSurface,
    PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy, Window,
    WindowSurfaceType,
};
use smithay::input::pointer::{CursorImageStatus, Focus};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::ImportDma;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::fractional_scale::{with_fractional_scale, FractionalScaleHandler};
use smithay::wayland::selection::primary_selection::{PrimarySelectionHandler, PrimarySelectionState};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::tablet_manager::TabletSeatHandler;
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::wayland::shell::wlr_layer::{
    KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData,
    WlrLayerShellHandler, WlrLayerShellState,
};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Resource};
use smithay::utils::{Rectangle, Serial};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    get_parent, is_sync_subsurface, with_states, CompositorClientState, CompositorHandler,
    CompositorState,
};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState,
    ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_fractional_scale,
    delegate_layer_shell, delegate_output, delegate_primary_selection, delegate_seat,
    delegate_shm, delegate_viewporter, delegate_xdg_activation, delegate_xdg_decoration,
    delegate_xdg_shell,
};

use super::grabs::{MoveGrab, ResizeGrab, ResizeTracking};
use super::{ClientState, Helix};

/// How long an activation token stays valid.
const ACTIVATION_TOKEN_TTL: std::time::Duration = std::time::Duration::from_secs(10);

impl CompositorHandler for Helix {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(xwayland) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &xwayland.compositor_state;
        }
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        self.add_commit_blockers(surface);
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        helix_comp::color_management::commit(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self
                .space
                .elements()
                .find(|w| super::wm::surface_of(w).as_ref() == Some(&root))
            {
                window.on_commit();
            }
        }
        self.handle_xdg_commit(surface);
        if let Some(window) = self.wm.unplaced.iter().find(|w| super::wm::surface_of(w).as_ref() == Some(surface)).cloned() {
            if window.geometry().size.w > 0 {
                self.place_new_window(&window);
            }
        }
        self.handle_layer_commit(surface);
        self.track_resize_commit(surface);
    }
}

impl BufferHandler for Helix {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for Helix {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl SeatHandler for Helix {
    type KeyboardFocus = super::focus::KeyboardFocusTarget;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&super::focus::KeyboardFocusTarget>) {
        use smithay::wayland::seat::WaylandFocus;
        let dh = &self.display_handle;
        let surface = focused.and_then(|f| f.wl_surface()).map(|s| s.into_owned());
        let client = surface.as_ref().and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        smithay::wayland::selection::primary_selection::set_primary_focus(dh, seat, client);
        self.update_shortcut_inhibit(surface);
    }

    fn led_state_changed(&mut self, _seat: &Seat<Self>, led_state: smithay::input::keyboard::LedState) {
        self.update_leds(led_state);
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
    }
}

impl SelectionHandler for Helix {
    type SelectionUserData = super::ServerSelection;

    /// A Wayland client set the clipboard/primary selection: offer it to X11.
    fn new_selection(
        &mut self,
        ty: smithay::wayland::selection::SelectionTarget,
        source: Option<smithay::wayland::selection::SelectionSource>,
        _seat: Seat<Self>,
    ) {
        let mime_types = source.as_ref().map(|s| s.mime_types());
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.new_selection(ty, mime_types.clone()) {
                tracing::warn!(?err, ?ty, "cannot offer selection to X11");
            }
        }
        if ty == smithay::wayland::selection::SelectionTarget::Clipboard {
            self.clipboard_changed(mime_types.unwrap_or_default(), None);
        }
    }

    /// A Wayland client wants the selection an X11 app owns.
    fn send_selection(
        &mut self,
        ty: smithay::wayland::selection::SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        user_data: &super::ServerSelection,
    ) {
        if let super::ServerSelection::Image(png) = user_data {
            super::capture::serve_image(png.clone(), &mime_type, fd);
            return;
        }
        if let super::ServerSelection::Text(text) = user_data {
            super::capture::serve_text(text.clone(), &mime_type, fd);
            return;
        }
        if let super::ServerSelection::Remote { session } = user_data {
            self.start_transfer(*session, mime_type, fd);
            return;
        }
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.send_selection(ty, mime_type, fd, self.loop_handle.clone()) {
                tracing::warn!(?err, "cannot transfer selection from X11");
            }
        }
    }
}

impl DataDeviceHandler for Helix {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl PrimarySelectionHandler for Helix {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

impl ClientDndGrabHandler for Helix {}
impl ServerDndGrabHandler for Helix {}
impl OutputHandler for Helix {}

impl XdgShellHandler for Helix {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn client_pong(&mut self, client: smithay::wayland::shell::xdg::ShellClient) {
        self.pong(&client);
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        tracing::info!("new toplevel window");
        let window = Window::new_wayland_window(surface);
        let position = self.next_window_position();
        // Placed for real once it has a size (its first buffer).
        self.wm.unplaced.push(window.clone());
        self.map_new_window(&window, position.into());
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn grab(&mut self, surface: PopupSurface, seat: WlSeat, serial: Serial) {
        if self.lock.locked {
            return;
        }
        let Some(seat) = Seat::<Self>::from_resource(&seat) else { return };
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else { return };
        if self.window_for_surface(&root).is_none() {
            return;
        }
        let Ok(mut grab) = self.popups.grab_popup(root.into(), kind, &seat, serial) else {
            return;
        };
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial)
                    || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: WlSeat, serial: Serial) {
        let Some(seat) = Seat::<Self>::from_resource(&seat) else { return };
        let Some(start_data) = check_grab(&seat, surface.wl_surface(), serial) else { return };
        let Some(window) = self.window_for_surface(surface.wl_surface()) else { return };
        // Dragging a maximized window un-maximizes it first.
        self.restore_window(&window);
        let initial_location = self.space.element_location(&window).unwrap_or_default();
        let pointer = seat.get_pointer().unwrap();
        pointer.set_grab(
            self,
            MoveGrab {
                start_data,
                window,
                initial_location,
                follow: false,
                unmaximize: false,
            },
            serial,
            Focus::Clear,
        );
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let Some(seat) = Seat::<Self>::from_resource(&seat) else { return };
        let Some(start_data) = check_grab(&seat, surface.wl_surface(), serial) else { return };
        let Some(window) = self.window_for_surface(surface.wl_surface()) else { return };
        let location = self.space.element_location(&window).unwrap_or_default();
        let initial = smithay::utils::Rectangle::new(location, window.geometry().size);
        let edges = edges.into();
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Resizing);
        });
        surface.send_pending_configure();
        self.resize = Some(ResizeTracking {
            window: window.clone(),
            edges,
            initial,
            finished: false,
        });
        let pointer = seat.get_pointer().unwrap();
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
            serial,
            Focus::Clear,
        );
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.maximize(&window);
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.restore_window(&window);
        }
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, output: Option<WlOutput>) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            let on = output.as_ref().and_then(Output::from_resource);
            self.fullscreen(&window, on);
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.restore_window(&window);
        }
    }

    /// The app asks for the window menu (e.g. a right click on its header
    /// bar), at `location` relative to its surface.
    fn show_window_menu(
        &mut self,
        surface: ToplevelSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
        location: smithay::utils::Point<i32, smithay::utils::Logical>,
    ) {
        let Some(window) = self.window_for_surface(surface.wl_surface()) else { return };
        let Some(at) = self.space.element_location(&window) else { return };
        self.send_window_menu(&window, at - window.geometry().loc + location);
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.minimize(&window);
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.window_gone(surface.wl_surface());
    }
}

/// The pointer grab a client's move/resize request refers to, if the
/// request is legitimate (the pointer is pressed on that client).
fn check_grab(
    seat: &Seat<Helix>,
    surface: &WlSurface,
    serial: Serial,
) -> Option<smithay::input::pointer::GrabStartData<Helix>> {
    let pointer = seat.get_pointer()?;
    if !pointer.has_grab(serial) {
        return None;
    }
    let start_data = pointer.grab_start_data()?;
    let (focus, _) = start_data.focus.as_ref()?;
    if !focus.id().same_client_as(&surface.id()) {
        return None;
    }
    Some(start_data)
}

impl XdgDecorationHandler for Helix {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        // Helix draws no titlebars; clients decorate themselves (GNOME-style).
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ClientSide);
        });
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        self.new_decoration(toplevel.clone());
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.request_mode(toplevel, DecorationMode::ClientSide);
    }
}

impl XdgActivationHandler for Helix {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.activation_state
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // Honor fresh tokens (e.g. an app launched from the shell, or a
        // notification click); ignore stale ones rather than steal focus.
        if let Some(window) = self.window_for_surface(&surface) {
            if token_data.timestamp.elapsed() < ACTIVATION_TOKEN_TTL {
                self.focus_window(&window);
            } else {
                // Not granted: it wants attention instead.
                self.set_attention(&window, true);
            }
        }
        self.activation_state.remove_token(&token);
    }
}

impl FractionalScaleHandler for Helix {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self.initial_surface_scale();
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| fractional.set_preferred_scale(scale));
        });
        self.update_surface_scales();
    }
}

impl WlrLayerShellHandler for Helix {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let output = output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.primary());
        let Some(output) = output else {
            tracing::warn!(namespace, "layer surface without an output; ignoring");
            return;
        };
        tracing::info!(namespace, "new layer surface");
        if !self.popover_opened(&namespace, surface.wl_surface()) {
            surface.send_close();
            return;
        }
        // The shell's always-there surfaces appear without fading.
        if helix_comp::animation::layer_animates(&namespace) {
            self.animations.layer_opened(self.wm_settings.motion, surface.wl_surface());
        }
        let mut map = layer_map_for_output(&output);
        if let Err(err) = map.map_layer(&LayerSurface::new(surface, namespace)) {
            tracing::warn!(?err, "failed to map layer surface");
        }
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let found = self.space.outputs().find_map(|o| {
            let map = layer_map_for_output(o);
            let layer = map.layers().find(|l| l.layer_surface() == &surface).cloned();
            layer.map(|layer| (o.clone(), layer))
        });
        if let Some((output, layer)) = found {
            let geometry = layer_map_for_output(&output).layer_geometry(&layer);
            let shell_chrome = !helix_comp::animation::layer_animates(layer.namespace());
            if let (Some(geometry), Some(area), false) = (geometry, self.space.output_geometry(&output), shell_chrome) {
                let rect = Rectangle::new(geometry.loc + area.loc, geometry.size).to_f64();
                self.animations.leave(self.wm_settings.motion, surface.wl_surface(), rect, rect.loc, helix_comp::animation::Kind::LayerOut, rect.loc, 0, true);
            }
            layer_map_for_output(&output).unmap_layer(&layer);
        }
        // Give focus back to the windows if the layer surface had it.
        if self.focused_surface().as_ref() == Some(surface.wl_surface()) {
            match self.top_window() {
                Some(top) => self.focus_window(&top),
                None => self.unfocus_all(),
            }
        }
    }
}

impl Helix {
    /// Lay out layer surfaces and send their first configure.
    fn handle_layer_commit(&mut self, surface: &WlSurface) {
        let Some(output) = self
            .space
            .outputs()
            .find(|o| {
                layer_map_for_output(o)
                    .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                    .is_some()
            })
            .cloned()
        else {
            return;
        };
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<LayerSurfaceData>()
                .map(|data| data.lock().unwrap().initial_configure_sent)
                .unwrap_or(true)
        });
        self.popover_commit(surface);
        let wants_keyboard = {
            let mut map = layer_map_for_output(&output);
            map.arrange();
            let layer = map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL);
            if !initial_configure_sent {
                if let Some(layer) = layer {
                    layer.layer_surface().send_configure();
                }
            }
            // Top/overlay surfaces asking for exclusive keyboard focus (e.g.
            // the launcher) get it while mapped, per wlr-layer-shell.
            layer.is_some_and(|l| {
                let cached = l.cached_state();
                cached.keyboard_interactivity == KeyboardInteractivity::Exclusive
                    && matches!(cached.layer, Layer::Top | Layer::Overlay)
            })
        };
        self.constrain_to_work_area();
        if initial_configure_sent && wants_keyboard && !self.lock.locked {
            let keyboard = self.seat.get_keyboard().unwrap();
            if self.focused_surface().as_ref() != Some(surface) {
                keyboard.set_focus(self, Some(surface.clone().into()), smithay::utils::SERIAL_COUNTER.next_serial());
            }
        }
    }

    fn handle_xdg_commit(&mut self, surface: &WlSurface) {
        if let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
            .cloned()
        {
            let initial_configure_sent = with_states(surface, |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .initial_configure_sent
            });
            if !initial_configure_sent {
                window.toplevel().unwrap().send_configure();
            }
        }

        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(xdg)) = self.popups.find_popup(surface) {
            if !xdg.is_initial_configure_sent() {
                let _ = xdg.send_configure();
            }
        }
    }

    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &root))
        else {
            return;
        };
        let Some(output) = self.output_of(window) else {
            return;
        };
        let (Some(output_geo), Some(window_geo)) = (
            self.space.output_geometry(&output),
            self.space.element_geometry(window),
        ) else {
            return;
        };
        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;
        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}

delegate_compositor!(Helix);
delegate_shm!(Helix);
delegate_seat!(Helix);
delegate_data_device!(Helix);
delegate_output!(Helix);
delegate_xdg_shell!(Helix);
delegate_layer_shell!(Helix);
delegate_primary_selection!(Helix);
delegate_xdg_decoration!(Helix);
delegate_xdg_activation!(Helix);
delegate_viewporter!(Helix);
delegate_fractional_scale!(Helix);
delegate_dmabuf!(Helix);
smithay::delegate_xdg_system_bell!(Helix);
smithay::delegate_xdg_dialog!(Helix);

/// `xdg_system_bell_v1`: an app rang the bell.
impl smithay::wayland::xdg_system_bell::XdgSystemBellHandler for Helix {
    fn ring(&mut self, surface: Option<WlSurface>) {
        self.ring_bell(surface.as_ref());
    }
}

/// `xdg_dialog_v1`: which dialogs are modal (for `attach-modal-dialogs`).
impl smithay::wayland::shell::xdg::dialog::XdgDialogHandler for Helix {
    fn modal_changed(&mut self, toplevel: ToplevelSurface, is_modal: bool) {
        let surface = toplevel.wl_surface().clone();
        self.wm.modal.retain(|s| s != &surface && s.is_alive());
        if is_modal {
            self.wm.modal.push(surface);
        }
    }
}
smithay::delegate_session_lock!(Helix);
smithay::delegate_xwayland_shell!(Helix);

impl DmabufHandler for Helix {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        // Test the import against the renderer that will use it; nested,
        // that renderer lives with the host window, which answers on its
        // next frame.
        let Some(udev) = self.udev.as_mut() else {
            self.pending_dmabufs.push((dmabuf, notifier));
            return;
        };
        let ok = udev.renderer().import_dmabuf(&dmabuf, None).is_ok();
        if ok {
            let _ = notifier.successful::<Helix>();
        } else {
            notifier.failed();
        }
    }
}

/// Required by cursor-shape (tablet tool cursors); defaults suffice.
impl TabletSeatHandler for Helix {}

impl helix_comp::tablet_v2::TabletV2Handler for Helix {
    fn tablet_v2(&mut self) -> &mut helix_comp::tablet_v2::TabletV2 {
        &mut self.tablet_v2
    }
    fn tool_cursor(&mut self, tool: u64, cursor: Option<(WlSurface, (i32, i32))>) {
        self.set_tool_cursor(tool, cursor);
    }
}

impl helix_comp::cursor_shape::CursorShapeHandler for Helix {
    /// Taken while one of the client's surfaces has the pointer (or
    /// started the pointer grab).
    fn pointer_shape(&mut self, client: smithay::reexports::wayland_server::backend::ClientId, icon: smithay::input::pointer::CursorIcon) {
        let Some(pointer) = self.seat.get_pointer() else { return };
        let of = |s: &WlSurface| s.client().map(|c| c.id());
        let focused = pointer.current_focus().and_then(|f| of(&f)) == Some(client.clone());
        let grab = pointer.grab_start_data().and_then(|d| d.focus).and_then(|(f, _)| of(&f)) == Some(client);
        tracing::debug!(?icon, taken = focused || grab, "cursor shape");
        if focused || grab {
            self.cursor_status = smithay::input::pointer::CursorImageStatus::Named(icon);
        }
    }

    fn tool_shape(&mut self, _client: smithay::reexports::wayland_server::backend::ClientId, tool: u64, icon: smithay::input::pointer::CursorIcon) {
        self.set_tool_shape(tool, icon);
    }
}

mod tablet_v2_delegates {
    use helix_comp::tablet_v2::TabletV2;
    use smithay::reexports::wayland_protocols::wp::tablet::zv2::server as t;
    use smithay::reexports::wayland_server::{delegate_dispatch, delegate_global_dispatch};

    use super::Helix;

    delegate_global_dispatch!(Helix: [t::zwp_tablet_manager_v2::ZwpTabletManagerV2: ()] => TabletV2);
    delegate_dispatch!(Helix: [t::zwp_tablet_manager_v2::ZwpTabletManagerV2: ()] => TabletV2);
    delegate_dispatch!(Helix: [t::zwp_tablet_seat_v2::ZwpTabletSeatV2: ()] => TabletV2);
    delegate_dispatch!(Helix: [t::zwp_tablet_v2::ZwpTabletV2: String] => TabletV2);
    delegate_dispatch!(Helix: [t::zwp_tablet_tool_v2::ZwpTabletToolV2: u64] => TabletV2);
    delegate_dispatch!(Helix: [t::zwp_tablet_pad_v2::ZwpTabletPadV2: String] => TabletV2);
    use helix_comp::cursor_shape::{CursorShapeManager, Device};
    use smithay::reexports::wayland_protocols::wp::cursor_shape::v1::server as cs;
    delegate_global_dispatch!(Helix: [cs::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1: ()] => CursorShapeManager);
    delegate_dispatch!(Helix: [cs::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1: ()] => CursorShapeManager);
    delegate_dispatch!(Helix: [cs::wp_cursor_shape_device_v1::WpCursorShapeDeviceV1: Device] => CursorShapeManager);
    delegate_dispatch!(Helix: [t::zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2: (String, usize)] => TabletV2);
    delegate_dispatch!(Helix: [t::zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2: (String, u32)] => TabletV2);
    delegate_dispatch!(Helix: [t::zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2: (String, u32)] => TabletV2);
}

impl smithay::wayland::input_method::InputMethodHandler for Helix {
    fn new_popup(&mut self, surface: smithay::wayland::input_method::PopupSurface) {
        if let Err(err) = self.popups.track_popup(smithay::desktop::PopupKind::from(surface)) {
            tracing::warn!(%err, "cannot track input method popup");
        }
    }

    fn dismiss_popup(&mut self, surface: smithay::wayland::input_method::PopupSurface) {
        if let Some(parent) = surface.get_parent().map(|p| p.surface.clone()) {
            let _ = smithay::desktop::PopupManager::dismiss_popup(&parent, &smithay::desktop::PopupKind::from(surface));
        }
    }

    fn popup_repositioned(&mut self, _surface: smithay::wayland::input_method::PopupSurface) {}

    fn parent_geometry(&self, parent: &WlSurface) -> smithay::utils::Rectangle<i32, smithay::utils::Logical> {
        use smithay::wayland::seat::WaylandFocus;
        self.space
            .elements()
            .find_map(|w| (w.wl_surface().as_deref() == Some(parent)).then(|| w.geometry()))
            .unwrap_or_default()
    }
}

smithay::delegate_text_input_manager!(Helix);
smithay::delegate_input_method_manager!(Helix);
smithay::delegate_virtual_keyboard_manager!(Helix);

smithay::delegate_presentation!(Helix);
smithay::delegate_single_pixel_buffer!(Helix);
smithay::delegate_fifo!(Helix);
smithay::delegate_commit_timing!(Helix);
