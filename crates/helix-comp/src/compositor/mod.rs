//! The Smithay compositor (winit backend for now).
//!
//! Structure follows Smithay's `smallvil` example (MIT licensed).

mod capture;
mod cursor;
mod grabs;
mod handlers;
mod input;
mod input_config;
mod keybindings;
mod lock;
pub mod monitors;
mod shortcuts;
mod supervise;
pub mod window_list;
pub mod udev;
pub mod winit;
mod wm;
mod xwayland;

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
use smithay::wayland::session_lock::SessionLockManagerState;
use smithay::wayland::xdg_activation::XdgActivationState;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::X11Wm;
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
    /// Last known usable area (output minus panels), to notice changes.
    pub work_area: Option<Rectangle<i32, Logical>>,
    pub shortcuts: shortcuts::Shortcuts,
    pub keybindings: keybindings::Keybindings,
    /// The shell we launched and keep running, if any.
    pub shell: Option<supervise::Shell>,
    /// Shell process exits, reported by waiter threads.
    shell_exits: Option<smithay::reexports::calloop::channel::Sender<std::process::ExitStatus>>,
    /// Cursor requested by the focused client, for the host window.
    pub cursor: Option<CursorIcon>,
    /// Full cursor state, drawn by the compositor on bare hardware.
    pub cursor_status: CursorImageStatus,
    pub dmabuf_state: DmabufState,
    /// Set when running on bare hardware (DRM/KMS).
    pub udev: Option<udev::UdevData>,
    /// The X11 window manager for Xwayland, once it is running.
    pub xwm: Option<X11Wm>,
    /// Xwayland's display number (`DISPLAY=:N`), once it is running.
    pub xdisplay: Option<u32>,
    pub xwayland_shell_state: XWaylandShellState,
    pub session_lock_state: SessionLockManagerState,
    pub lock: lock::LockState,
    /// Connector of the primary monitor, as last configured.
    pub primary_output: Option<String>,
    /// Input settings as last applied (`None` before the first time).
    pub input_settings: Option<helix_comp::input_settings::InputSettings>,
    /// libinput devices (bare hardware), to apply settings to.
    pub input_devices: Vec<smithay::reexports::input::Device>,
    /// What the shell knows about open windows.
    pub window_list: window_list::WindowList,
    /// Screen captures asked for by the shell.
    pub captures: capture::Captures,

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

        // Private protocol for the shell (visible to privileged clients only).
        dh.create_global::<Self, helix_protocols::server::helix_keybindings_manager_v1::HelixKeybindingsManagerV1, _>(2, ());
        dh.create_global::<Self, helix_protocols::server::helix_window_list_v1::HelixWindowListV1, _>(1, ());
        dh.create_global::<Self, helix_protocols::server::helix_screenshot_manager_v1::HelixScreenshotManagerV1, _>(1, ());
        let socket_name = Self::listen(display, event_loop)?;
        let mut helix = Self {
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
            work_area: None,
            shortcuts: shortcuts::Shortcuts::default(),
            keybindings: keybindings::Keybindings::default(),
            shell: None,
            shell_exits: None,
            cursor: Some(CursorIcon::Default),
            cursor_status: CursorImageStatus::default_named(),
            dmabuf_state: DmabufState::new(),
            udev: None,
            xwm: None,
            xdisplay: None,
            xwayland_shell_state: XWaylandShellState::new::<Self>(&dh),
            session_lock_state: SessionLockManagerState::new::<Self, _>(&dh, lock::can_lock),
            lock: lock::LockState::default(),
            events,
            last_activity_sent: None,
            windows_mapped: 0,
            primary_output: None,
            input_settings: None,
            input_devices: Vec::new(),
            window_list: Default::default(),
            captures: Default::default(),
        };
        helix.init_captures();
        Ok(helix)
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

    /// Connect the shell on a pre-connected socket, as a privileged client,
    /// and start it with `WAYLAND_SOCKET` pointing at the other end.
    pub fn spawn_privileged(&mut self, program: &str) -> std::io::Result<std::process::Child> {
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        use std::os::unix::process::CommandExt;

        let (ours, theirs) = UnixStream::pair()?;
        self.display_handle
            .insert_client(
                ours,
                Arc::new(ClientState {
                    privileged: true,
                    ..ClientState::default()
                }),
            )
            .map_err(std::io::Error::other)?;
        let fd = theirs.as_raw_fd();
        let mut cmd = std::process::Command::new(program);
        cmd.env("WAYLAND_SOCKET", fd.to_string());
        // A shell (re)started while locked must lock again straight away.
        if self.lock.locked {
            cmd.env("HELIX_LOCKED", "1");
        } else {
            cmd.env_remove("HELIX_LOCKED");
        }
        self.set_child_display(&mut cmd);
        // SAFETY: only async-signal-safe fcntl calls between fork and exec,
        // to let the child inherit its end of the socket.
        unsafe {
            cmd.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        drop(theirs);
        Ok(child)
    }

    /// Start `program` as the shell and restart it whenever it exits.
    pub fn supervise_shell(&mut self, program: String) -> Result<(), Box<dyn std::error::Error>> {
        let (tx, rx) = smithay::reexports::calloop::channel::channel();
        self.loop_handle
            .insert_source(rx, |event, _, state| {
                if let smithay::reexports::calloop::channel::Event::Msg(status) = event {
                    state.shell_exited(status);
                }
            })
            .map_err(|e| format!("cannot watch the shell: {e}"))?;
        self.shell_exits = Some(tx);
        self.shell = Some(supervise::Shell::new(program));
        self.start_shell();
        Ok(())
    }

    fn start_shell(&mut self) {
        let Some(program) = self.shell.as_ref().map(|s| s.program.clone()) else { return };
        match self.spawn_privileged(&program) {
            Ok(mut child) => {
                let pid = child.id();
                tracing::info!(program, pid, "started shell");
                if let Some(shell) = self.shell.as_mut() {
                    shell.pid = Some(pid);
                }
                let tx = self.shell_exits.clone();
                std::thread::spawn(move || {
                    if let (Ok(status), Some(tx)) = (child.wait(), tx) {
                        let _ = tx.send(status);
                    }
                });
            }
            Err(err) => {
                tracing::error!(%err, program, "cannot start shell");
                // Treat like a crash so the backoff and give-up rules apply.
                self.schedule_shell_restart();
            }
        }
    }

    fn shell_exited(&mut self, status: std::process::ExitStatus) {
        if let Some(shell) = self.shell.as_mut() {
            shell.pid = None;
        }
        tracing::warn!(%status, "shell exited");
        self.schedule_shell_restart();
    }

    fn schedule_shell_restart(&mut self) {
        let Some(shell) = self.shell.as_mut() else { return };
        match shell.policy.next_delay(Instant::now()) {
            Some(delay) => {
                tracing::info!(?delay, "restarting shell");
                let timer = smithay::reexports::calloop::timer::Timer::from_duration(delay);
                let _ = self.loop_handle.insert_source(timer, |_, _, state| {
                    state.start_shell();
                    smithay::reexports::calloop::timer::TimeoutAction::Drop
                });
            }
            None => tracing::error!(
                "shell exited {} times within {:?}; not restarting it again",
                supervise::MAX_RESTARTS,
                supervise::WINDOW
            ),
        }
    }

    /// Stop supervising and ask the shell to exit (compositor shutdown).
    pub fn stop_shell(&mut self) {
        if let Some(mut shell) = self.shell.take() {
            shell.terminate();
        }
    }

    /// Point a child at our Xwayland, never at a host X server (when
    /// nested, our own DISPLAY is the host's).
    pub fn set_child_display(&self, cmd: &mut std::process::Command) {
        match self.xdisplay {
            Some(n) => cmd.env("DISPLAY", format!(":{n}")),
            None => cmd.env_remove("DISPLAY"),
        };
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

    /// The output at `pos`, else the first one.
    fn output_at(&self, pos: Point<f64, Logical>) -> Option<&smithay::output::Output> {
        self.space.output_under(pos).next().or_else(|| self.space.outputs().next())
    }

    /// Whether `pos` is over a panel-like layer surface (top or overlay).
    pub fn over_panel(&self, pos: Point<f64, Logical>) -> bool {
        let Some(output) = self.output_at(pos) else { return false };
        let Some(geo) = self.space.output_geometry(output) else { return false };
        let layers = layer_map_for_output(output);
        let local = pos - geo.loc.to_f64();
        layers.layer_under(Layer::Overlay, local).is_some() || layers.layer_under(Layer::Top, local).is_some()
    }

    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        if self.lock.locked {
            return self.lock_surface_under(pos);
        }
        let output = self.output_at(pos)?;
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
    /// The shell launched by the compositor (may use private protocols).
    pub privileged: bool,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
