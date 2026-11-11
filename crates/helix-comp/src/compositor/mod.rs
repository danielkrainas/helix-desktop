//! The Smithay compositor (winit backend for now).
//!
//! Structure follows Smithay's `smallvil` example (MIT licensed).

mod cursor;
mod grabs;
mod handlers;
mod input;
mod shortcuts;
pub mod udev;
pub mod winit;
mod wm;

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Instant;

use helix_comp::Event;
use smithay::desktop::{layer_map_for_output, PopupManager, Space, Window, WindowSurfaceType};
use smithay::input::pointer::{CursorIcon, CursorImageStatus};
use smithay::input::{Seat, SeatState};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::DmabufState;
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::xdg_activation::XdgActivationState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::wlr_layer::{Layer, WlrLayerShellState};
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;
use tokio::sync::mpsc::UnboundedSender;

/// Minimum spacing between activity events sent to the IdleMonitor.
const ACTIVITY_THROTTLE_MS: u128 = 50;

pub struct Helix {
    pub start_time: Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub space: Space<Window>,
    pub loop_signal: LoopSignal,
    pub loop_handle: LoopHandle<'static, Helix>,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub layer_shell_state: WlrLayerShellState,
    pub shm_state: ShmState,
    /// Kept alive so the wl_output / xdg-output globals stay registered.
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Helix>,
    pub data_device_state: DataDeviceState,
    pub popups: PopupManager,
    pub seat: Seat<Self>,
    pub primary_selection_state: PrimarySelectionState,
    pub activation_state: XdgActivationState,
    /// Kept alive so their globals stay registered.
    #[allow(dead_code)]
    globals: (
        ViewporterState,
        FractionalScaleManagerState,
        CursorShapeManagerState,
        XdgDecorationState,
    ),

    /// Minimized windows (unmapped) and where they were.
    pub minimized: Vec<Window>,
    pub minimized_at: Vec<Point<i32, Logical>>,
    /// Geometry to restore after maximize/fullscreen.
    pub restore: Vec<(Window, Rectangle<i32, Logical>, wm::Takeover)>,
    /// Interactive resize awaiting the client's commits.
    pub resize: Option<grabs::ResizeTracking>,
    pub shortcuts: shortcuts::Shortcuts,
    /// Cursor requested by the focused client, for the host window.
    pub cursor: Option<CursorIcon>,
    /// Full cursor state, drawn by the compositor on bare hardware.
    pub cursor_status: CursorImageStatus,
    pub dmabuf_state: DmabufState,
    /// Set when running on bare hardware (DRM/KMS).
    pub udev: Option<udev::UdevData>,

    events: UnboundedSender<Event>,
    last_activity_sent: Option<Instant>,
    windows_mapped: u32,
}

impl Helix {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        events: UnboundedSender<Event>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let dh = display.handle();
        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "seat0");
        seat.add_keyboard(Default::default(), 200, 25)?;
        seat.add_pointer();

        let socket_name = Self::listen(display, event_loop)?;
        Ok(Self {
            start_time: Instant::now(),
            socket_name,
            compositor_state: CompositorState::new::<Self>(&dh),
            xdg_shell_state: XdgShellState::new::<Self>(&dh),
            layer_shell_state: WlrLayerShellState::new::<Self>(&dh),
            shm_state: ShmState::new::<Self>(&dh, vec![]),
            output_manager_state: OutputManagerState::new_with_xdg_output::<Self>(&dh),
            data_device_state: DataDeviceState::new::<Self>(&dh),
            display_handle: dh.clone(),
            space: Space::default(),
            loop_signal: event_loop.get_signal(),
            loop_handle: event_loop.handle(),
            primary_selection_state: PrimarySelectionState::new::<Self>(&dh),
            activation_state: XdgActivationState::new::<Self>(&dh),
            globals: (
                ViewporterState::new::<Self>(&dh),
                FractionalScaleManagerState::new::<Self>(&dh),
                CursorShapeManagerState::new::<Self>(&dh),
                XdgDecorationState::new::<Self>(&dh),
            ),
            seat_state,
            popups: PopupManager::default(),
            seat,
            minimized: Vec::new(),
            minimized_at: Vec::new(),
            restore: Vec::new(),
            resize: None,
            shortcuts: shortcuts::Shortcuts::default(),
            cursor: Some(CursorIcon::Default),
            cursor_status: CursorImageStatus::default_named(),
            dmabuf_state: DmabufState::new(),
            udev: None,
            events,
            last_activity_sent: None,
            windows_mapped: 0,
        })
    }

    fn listen(
        display: Display<Self>,
        event_loop: &mut EventLoop<'static, Self>,
    ) -> Result<OsString, Box<dyn std::error::Error>> {
        let socket = ListeningSocketSource::new_auto()?;
        let name = socket.socket_name().to_os_string();
        let handle = event_loop.handle();
        handle.insert_source(socket, |stream, _, state| {
            if let Err(err) = state
                .display_handle
                .insert_client(stream, Arc::new(ClientState::default()))
            {
                tracing::warn!(%err, "failed to add Wayland client");
            }
        })?;
        handle.insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                // SAFETY: the display is never dropped while the loop runs.
                unsafe {
                    display.get_mut().dispatch_clients(state).unwrap();
                }
                Ok(PostAction::Continue)
            },
        )?;
        Ok(name)
    }

    pub fn send(&self, event: Event) {
        // The D-Bus side may be absent (no session bus); that's fine.
        let _ = self.events.send(event);
    }

    /// Tell the IdleMonitor about user input, at most every few ms.
    pub fn note_activity(&mut self) {
        let now = Instant::now();
        let due = self
            .last_activity_sent
            .is_none_or(|t| now.duration_since(t).as_millis() >= ACTIVITY_THROTTLE_MS);
        if due {
            self.last_activity_sent = Some(now);
            self.send(Event::Activity);
        }
    }

    /// Where to put the next new window: a simple cascade inside the area
    /// not reserved by panels.
    pub fn next_window_position(&mut self) -> (i32, i32) {
        let origin = self
            .space
            .outputs()
            .next()
            .map(|o| layer_map_for_output(o).non_exclusive_zone().loc)
            .unwrap_or_default();
        let n = (self.windows_mapped % 10) as i32;
        self.windows_mapped += 1;
        (origin.x + 32 * n, origin.y + 32 * n)
    }

    /// Whether `pos` is over a panel-like layer surface (top or overlay).
    pub fn over_panel(&self, pos: Point<f64, Logical>) -> bool {
        let Some(output) = self.space.outputs().next() else { return false };
        let Some(geo) = self.space.output_geometry(output) else { return false };
        let layers = layer_map_for_output(output);
        let local = pos - geo.loc.to_f64();
        layers.layer_under(Layer::Overlay, local).is_some() || layers.layer_under(Layer::Top, local).is_some()
    }

    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        let output = self.space.outputs().next()?;
        let output_loc = self.space.output_geometry(output)?.loc;
        let layers = layer_map_for_output(output);
        let layer_hit = |layer: Layer| {
            let surface = layers.layer_under(layer, pos - output_loc.to_f64())?;
            let geo = layers.layer_geometry(surface)?;
            let loc = geo.loc + output_loc;
            surface
                .surface_under(pos - loc.to_f64(), WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p + loc).to_f64()))
        };
        layer_hit(Layer::Overlay)
            .or_else(|| layer_hit(Layer::Top))
            .or_else(|| {
                self.space.element_under(pos).and_then(|(window, location)| {
                    window
                        .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                        .map(|(s, p)| (s, (p + location).to_f64()))
                })
            })
            .or_else(|| layer_hit(Layer::Bottom))
            .or_else(|| layer_hit(Layer::Background))
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
