//! The Smithay compositor (winit backend for now).
//!
//! Structure follows Smithay's `smallvil` example (MIT licensed).

mod a11y;
mod agent_input;
mod actions;
mod zoom;
mod alive;
mod attention;
mod capture;
mod clipboard;
mod color;
mod cursor;
mod focus;
mod frames;
mod gamma;
mod grabs;
mod gtk_shell;
mod handlers;
mod input;
mod tablet_input;
mod color_surfaces;
mod animations;
mod hdr;
pub mod libinput_source;
mod remote;
mod protocols;
mod remote_share;
mod icons;
mod scales;
mod x11ping;
mod sync;
mod tearing;
mod timing;
mod tiling;
mod wm_behavior;
mod input_config;
mod keybindings;
mod lock;
mod safe_mode;
mod ext_popover;
pub mod pipewire;
mod screencast;
pub mod monitors;
mod shortcuts;
mod supervise;
pub mod window_list;
pub mod headless;
mod headless_input;
pub mod udev;
pub mod winit;
mod wm;
mod workspaces;
#[cfg(test)]
mod placement_tests;
mod xwayland;
mod xsettings;
mod input_capture;

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

/// A relative pointer motion: from where, unaccelerated delta, time (ms).
pub type RelativeMotion = (Point<f64, Logical>, Point<f64, Logical>, u64);

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
        helix_comp::cursor_shape::CursorShapeManager,
        XdgDecorationState,
        smithay::wayland::text_input::TextInputManagerState,
        smithay::wayland::input_method::InputMethodManagerState,
        smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState,
        smithay::wayland::xdg_system_bell::XdgSystemBellState,
        smithay::wayland::shell::xdg::dialog::XdgDialogState,
    ),
    #[allow(dead_code)]
    protocol_globals: protocols::Globals,
    pub keyboard_shortcuts_inhibit_state: smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState,
    pub xdg_foreign_state: smithay::wayland::xdg_foreign::XdgForeignState,
    /// The clock frame times are given in (`wp_presentation`).
    pub clock: smithay::utils::Clock<smithay::utils::Monotonic>,
    pub protocols: protocols::ProtocolState,

    /// Minimized windows (unmapped) and where they were.
    pub minimized: Vec<Window>,
    pub minimized_at: Vec<Point<i32, Logical>>,
    /// Geometry to restore after maximize/fullscreen.
    pub restore: Vec<(Window, Rectangle<i32, Logical>, wm::Takeover)>,
    /// Interactive resize awaiting the client's commits.
    pub resize: Option<grabs::ResizeTracking>,
    /// Always on Top windows.
    pub above: Vec<Window>,
    /// A Move/Resize following the pointer: window, geometry before, resize.
    pub follow: Option<(Window, Rectangle<i32, Logical>, bool)>,
    /// Windows `show-desktop` minimized.
    pub shown_desktop: Option<Vec<Window>>,
    /// Last known usable areas (outputs minus panels), to notice changes.
    pub work_areas: Vec<(String, Rectangle<i32, Logical>)>,
    pub shortcuts: shortcuts::Shortcuts,
    pub keybindings: keybindings::Keybindings,
    /// The shell we launched and keep running, if any.
    pub shell: Option<supervise::Shell>,
    /// A shell exiting with status 0 ends the compositor (`--exit-with-shell`).
    pub exit_with_shell: bool,
    /// Shift was held when the shell first started: extensions are off
    /// for the session (`None` until checked).
    pub extensions_off: Option<bool>,
    /// Shell process exits, reported by waiter threads.
    shell_exits: Option<smithay::reexports::calloop::channel::Sender<std::process::ExitStatus>>,
    /// What the host window's pointer was last set to (nested): a named
    /// cursor, or hidden (`None`) while Helix draws an app's image.
    pub host_cursor: Option<Option<CursorIcon>>,
    /// The pointer image apps (or Helix's frames) asked for.
    pub cursor_status: CursorImageStatus,
    /// Named cursors from the theme.
    pub cursors: cursor::Cursors,
    /// Window icons of X11 apps.
    pub icons: icons::IconState,
    /// `_NET_WM_PING` for X11 apps, once Xwayland runs.
    pub x11ping: Option<x11ping::X11Ping>,
    /// `wp_color_manager_v1`.
    pub color_manager: helix_comp::color_management::ColorManager,
    /// Explicit sync, when the GPU supports it.
    pub syncobj_state: Option<smithay::wayland::drm_syncobj::DrmSyncobjState>,
    /// Remote desktop clipboard sharing and keymaps.
    pub remote_share: remote_share::RemoteShare,
    /// The clipboard's generation and source (Agent Access).
    pub clipboard: clipboard::ClipboardState,
    pub dmabuf_state: DmabufState,
    /// GPU buffers waiting for the nested renderer to test them.
    pub pending_dmabufs: Vec<(smithay::backend::allocator::dmabuf::Dmabuf, smithay::wayland::dmabuf::ImportNotifier)>,
    /// Set when running on bare hardware (DRM/KMS).
    pub udev: Option<udev::UdevData>,
    /// Set by the headless backend (tests).
    pub headless: bool,
    /// The headless backend's GPU (`HELIX_HEADLESS_GPU`), if any.
    pub headless_gpu: Option<Box<headless::HeadlessGpu>>,
    /// The X11 window manager for Xwayland, once it is running.
    pub xwm: Option<X11Wm>,
    /// Xwayland's display number (`DISPLAY=:N`), once it is running.
    pub xdisplay: Option<u32>,
    /// Xwayland's Wayland client, once it is running.
    pub xwayland_client: Option<smithay::reexports::wayland_server::Client>,
    /// The XSETTINGS manager on Xwayland.
    pub xsettings: Option<xsettings::XSettings>,
    /// What X11 apps are told (XSETTINGS, X resources, the scale setting).
    pub x11_settings: helix_comp::xsettings::Settings,
    /// Xwayland's scale as last applied (0: not yet).
    pub xwayland_scale: i32,
    /// Input capture sessions and the capture going on.
    pub input_capture: input_capture::InputCaptureState,
    pub xwayland_shell_state: XWaylandShellState,
    pub session_lock_state: SessionLockManagerState,
    pub lock: lock::LockState,
    /// Connector of the primary monitor, as last configured.
    pub primary_output: Option<String>,
    /// Input settings as last applied (`None` before the first time).
    pub input_settings: Option<helix_comp::input_settings::InputSettings>,
    /// libinput devices (bare hardware), to apply settings to.
    pub input_devices: Vec<smithay::reexports::input::Device>,
    /// Num Lock as last remembered for the next session (None until the
    /// settings were first applied).
    pub saved_numlock: Option<bool>,
    /// Windows that want attention.
    pub attention: Vec<Window>,
    /// Touchscreen and tablet mapping state.
    pub tablet: tablet_input::TabletInput,
    /// The tablet protocol's objects.
    pub tablet_v2: helix_comp::tablet_v2::TabletV2,
    /// Touchscreen gestures in progress.
    pub touch_gestures: helix_comp::gestures::Recognizer,
    /// Windows, shell surfaces and workspaces animating.
    pub animations: animations::Animations,
    /// Nested, with `HELIX_TEST_HDR`: the HDR pass on the window.
    pub hdr_test: Option<hdr::HdrPass>,
    /// What the shell knows about open windows.
    pub window_list: window_list::WindowList,
    /// Screen captures asked for by the shell.
    pub captures: capture::Captures,
    /// Agent Access's synthetic input runs.
    pub agent_input: agent_input::AgentInputState,
    pub workspaces: workspaces::WorkspaceState,
    /// Title bars of X11 windows.
    pub frames: frames::Frames,
    /// Layer surfaces a click on a window took the keyboard from.
    pub layer_released: helix_comp::layer_focus::Released<WlSurface>,
    pub wm_settings: helix_comp::wm_settings::WmSettings,
    /// Keyboard accessibility (sticky, slow, bounce, mouse keys…).
    pub access: helix_comp::a11y_keys::AccessX,
    /// Screen readers' key watches and grabs (`org.freedesktop.a11y.KeyboardMonitor`).
    pub keyboard_monitor: helix_comp::keyboard_monitor::Shared,
    /// Keys grabbed for screen readers changed the modifiers since the
    /// focused app last heard of them.
    pub monitor_mods_dirty: bool,
    /// Mouse accessibility (secondary click, dwell click).
    pub mouse_access: helix_comp::a11y_mouse::Mouse,
    /// The magnifier.
    pub zoom: zoom::ZoomState,
    pub hot_corner: helix_comp::hot_corner::HotCorner,
    /// The relative motion being handled (from where, unaccelerated
    /// delta, time in ms), for the hot corner's pressure; `None` for
    /// absolute pointers.
    pub relative_motion: Option<RelativeMotion>,
    /// Pointer motion held back while the shell under the pointer is
    /// behind reading its socket, and the timer that sends it.
    pub held_motion: helix_comp::client_socket::MotionCoalescer,
    pub held_motion_timer: Option<smithay::reexports::calloop::RegistrationToken>,
    /// `PowerSaveMode` turned the monitors off.
    pub screens_off: bool,
    /// Where a dragged window would go if released now (edge tiling).
    pub tile_target: Option<wm::Takeover>,
    pub wm: wm_behavior::WmState,
    pub alive: alive::AliveState,
    pub remote: remote::RemoteState,
    pub gamma: gamma::GammaState,
    pub screencast: screencast::CastState,

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
        seat.add_touch();

        // Private protocol for the shell (visible to privileged clients only).
        dh.create_global::<Self, helix_protocols::server::helix_keybindings_manager_v1::HelixKeybindingsManagerV1, _>(2, ());
        dh.create_global::<Self, helix_protocols::server::helix_window_list_v1::HelixWindowListV1, _>(13, ());
        helix_comp::tablet_v2::TabletV2::global::<Self>(&dh);
        dh.create_global::<Self, helix_protocols::server::helix_screenshot_manager_v1::HelixScreenshotManagerV1, _>(8, ());
        let socket_name = Self::listen(display, event_loop)?;
        let mut helix = Self {
            start_time: Instant::now(),
            socket_name,
            compositor_state: CompositorState::new::<Self>(&dh),
            xdg_shell_state: XdgShellState::new::<Self>(&dh),
            // Not for apps in a sandbox: an overlay surface could take the
            // keyboard (and with it the clipboard) and draw over everything.
            layer_shell_state: WlrLayerShellState::new_with_filter::<Self, _>(&dh, |client| !client.get_data::<ClientState>().is_some_and(|c| c.sandboxed)),
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
                helix_comp::cursor_shape::CursorShapeManager::new::<Self>(&dh),
                XdgDecorationState::new::<Self>(&dh),
                smithay::wayland::text_input::TextInputManagerState::new::<Self>(&dh),
                // The input method and its virtual keyboard: the shell only.
                smithay::wayland::input_method::InputMethodManagerState::new::<Self, _>(&dh, privileged),
                smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState::new::<Self, _>(&dh, privileged),
                smithay::wayland::xdg_system_bell::XdgSystemBellState::new::<Self>(&dh),
                smithay::wayland::shell::xdg::dialog::XdgDialogState::new::<Self>(&dh),
            ),
            protocol_globals: {
                helix_comp::toplevel_icon::create_global::<Helix>(&dh, &icons::ICON_SIZES);
                helix_comp::tearing_control::create_global::<Helix>(&dh);
                gtk_shell::create_global(&dh);
                protocols::globals(&dh)
            },
            keyboard_shortcuts_inhibit_state: smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState::new::<Self>(&dh),
            xdg_foreign_state: smithay::wayland::xdg_foreign::XdgForeignState::new::<Self>(&dh),
            clock: smithay::utils::Clock::new(),
            protocols: Default::default(),
            seat_state,
            popups: PopupManager::default(),
            seat,
            minimized: Vec::new(),
            minimized_at: Vec::new(),
            restore: Vec::new(),
            resize: None,
            above: Vec::new(),
            follow: None,
            shown_desktop: None,
            work_areas: Vec::new(),
            shortcuts: shortcuts::Shortcuts::default(),
            keybindings: keybindings::Keybindings::default(),
            shell: None,
            exit_with_shell: false,
            extensions_off: None,
            shell_exits: None,
            host_cursor: None,
            cursors: cursor::Cursors::new("Adwaita", 24),
            remote_share: Default::default(),
            clipboard: Default::default(),
            syncobj_state: None,
            color_manager: helix_comp::color_management::ColorManager::new::<Self>(&dh),
            x11ping: None,
            icons: Default::default(),
            cursor_status: CursorImageStatus::default_named(),
            dmabuf_state: DmabufState::new(),
            pending_dmabufs: Vec::new(),
            udev: None,
            headless: false,
            headless_gpu: None,
            xwm: None,
            xdisplay: None,
            xwayland_client: None,
            xsettings: None,
            x11_settings: helix_comp::xsettings::read(),
            xwayland_scale: 0,
            input_capture: Default::default(),
            xwayland_shell_state: XWaylandShellState::new::<Self>(&dh),
            session_lock_state: SessionLockManagerState::new::<Self, _>(&dh, lock::can_lock),
            lock: lock::LockState::default(),
            events,
            last_activity_sent: None,
            windows_mapped: 0,
            primary_output: None,
            input_settings: None,
            input_devices: Vec::new(),
            saved_numlock: None,
            attention: Vec::new(),
            tablet: Default::default(),
            tablet_v2: Default::default(),
            touch_gestures: Default::default(),
            animations: Default::default(),
            hdr_test: std::env::var_os("HELIX_TEST_HDR").map(|_| Default::default()),
            window_list: Default::default(),
            captures: Default::default(),
            agent_input: Default::default(),
            workspaces: Default::default(),
            frames: Default::default(),
            layer_released: Default::default(),
            wm_settings: Default::default(),
            access: Default::default(),
            keyboard_monitor: Default::default(),
            monitor_mods_dirty: false,
            mouse_access: Default::default(),
            zoom: Default::default(),
            hot_corner: Default::default(),
            relative_motion: None,
            held_motion: Default::default(),
            held_motion_timer: None,
            screens_off: false,
            tile_target: None,
            wm: Default::default(),
            alive: Default::default(),
            remote: Default::default(),
            gamma: Default::default(),
            screencast: Default::default(),
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
            let data = Arc::new(ClientState::connected(&stream));
            if let Err(err) = state.display_handle.insert_client(stream, data) {
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
        // Room for a shell that stalls for a while: a large kernel send
        // buffer, and a large one in the server library (both only fill
        // while it is not reading).
        match helix_comp::client_socket::enlarge_send_buffer(std::os::fd::AsFd::as_fd(&ours), helix_comp::client_socket::SHELL_SEND_BUFFER) {
            Ok(granted) => tracing::info!(granted, "shell socket send buffer"),
            Err(err) => tracing::warn!(%err, "cannot enlarge the shell socket's send buffer"),
        }
        let data = Arc::new(ClientState {
            privileged: true,
            socket: std::sync::Mutex::new(ours.try_clone().ok().map(std::os::fd::OwnedFd::from)),
            ..ClientState::default()
        });
        let client = self.display_handle.insert_client(ours, data.clone()).map_err(std::io::Error::other)?;
        client.set_max_buffer_size(&self.display_handle, helix_comp::client_socket::SHELL_MAX_BUFFER);
        let fd = theirs.as_raw_fd();
        let mut cmd = std::process::Command::new(program);
        cmd.env("WAYLAND_SOCKET", fd.to_string());
        // Shift held at login: no extensions this session.
        if self.extensions_off == Some(true) {
            cmd.env("HELIX_EXTENSIONS_OFF", "1");
        } else {
            cmd.env_remove("HELIX_EXTENSIONS_OFF");
        }
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
        let command = std::path::Path::new(program).file_name().map_or_else(|| program.to_owned(), |n| n.to_string_lossy().into_owned());
        let _ = data.identity.set((child.id(), command));
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
        if self.extensions_off.is_none() {
            let held = self.shift_held_now();
            self.extensions_off = Some(held);
        }
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
        if self.exit_with_shell && status.success() {
            tracing::info!("shell finished; exiting");
            self.loop_signal.stop();
            return;
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
        match helix_comp::xauth::current().filter(|_| self.xdisplay.is_some()) {
            Some(auth) => cmd.env("XAUTHORITY", auth.path),
            None => cmd.env_remove("XAUTHORITY"),
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
            .pointer_output()
            .and_then(|o| self.usable_area_of(&o))
            .map(|a| a.loc)
            .unwrap_or_default();
        let n = (self.windows_mapped % 10) as i32;
        self.windows_mapped += 1;
        (origin.x + 32 * n, origin.y + 32 * n)
    }

    /// The output at `pos`, else the first one.
    pub(super) fn output_at(&self, pos: Point<f64, Logical>) -> Option<&smithay::output::Output> {
        self.space.output_under(pos).next().or_else(|| self.space.outputs().next())
    }

    /// Whether `pos` is over a panel-like layer surface (top or overlay).
    pub fn over_panel(&self, pos: Point<f64, Logical>) -> bool {
        let Some(output) = self.output_at(pos) else { return false };
        let Some(geo) = self.space.output_geometry(output) else { return false };
        let local = pos - geo.loc.to_f64();
        self.shown_layer_under(output, Layer::Overlay, local).is_some() || self.shown_layer_under(output, Layer::Top, local).is_some()
    }

    /// The `layer` surface of `output` under `local` (output coordinates)
    /// that is shown and takes input there: top-layer surfaces hidden by
    /// a full-screen window ([`wm::hidden_in_fullscreen`]) don't, and
    /// neither does a surface whose input region leaves the point out (a
    /// click-through overlay, such as a game's HUD: the point counts as
    /// over what is below it).
    pub(super) fn shown_layer_under(&self, output: &smithay::output::Output, layer: Layer, local: Point<f64, Logical>) -> Option<smithay::desktop::LayerSurface> {
        let fullscreen = layer == Layer::Top && self.in_fullscreen(output);
        let map = layer_map_for_output(output);
        let found = map
            .layers_on(layer)
            .rev()
            .filter(|l| !(fullscreen && wm::hidden_in_fullscreen(layer, l.namespace())))
            .find(|l| map.layer_geometry(l).is_some_and(|g| l.surface_under(local - g.loc.to_f64(), WindowSurfaceType::ALL).is_some()))
            .cloned();
        found
    }

    /// The window under `pos` and where it is: as the space has it, except
    /// that shaded windows are only their title bar (their contents let
    /// the pointer through).
    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&Window, Point<i32, Logical>)> {
        if self.frames.shaded.is_empty() {
            return self.space.element_under(pos);
        }
        self.space.elements().rev().filter(|w| !self.frames.shaded.contains(w)).find_map(|w| {
            let location = self.space.element_location(w)? - w.geometry().loc;
            smithay::desktop::space::SpaceElement::is_in_input_region(w, &(pos - location.to_f64())).then_some((w, location))
        })
    }

    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        if self.lock.locked {
            return self.lock_surface_under(pos);
        }
        let output = self.output_at(pos)?;
        let output_loc = self.space.output_geometry(output)?.loc;
        let layer_hit = |layer: Layer| {
            let surface = self.shown_layer_under(output, layer, pos - output_loc.to_f64())?;
            let geo = layer_map_for_output(output).layer_geometry(&surface)?;
            let loc = geo.loc + output_loc;
            surface
                .surface_under(pos - loc.to_f64(), WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p + loc).to_f64()))
        };
        layer_hit(Layer::Overlay)
            .or_else(|| layer_hit(Layer::Top))
            .or_else(|| {
                self.window_under(pos).and_then(|(window, location)| {
                    window
                        .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                        .map(|(s, p)| (s, (p + location).to_f64()))
                })
            })
            .or_else(|| layer_hit(Layer::Bottom))
            .or_else(|| layer_hit(Layer::Background))
    }
}

/// What a selection the compositor holds stands for.
#[derive(Debug, Clone)]
pub enum ServerSelection {
    /// An X11 app owns it (offered through Xwayland).
    X11,
    /// An image the compositor serves itself (screenshots), as PNG.
    Image(Arc<Vec<u8>>),
    /// Text the compositor serves itself (the shell's copies).
    Text(Arc<Vec<u8>>),
    /// A remote desktop session's clipboard content.
    Remote { session: u32 },
    /// Data of one type an agent copied (`copy_data`), other than plain
    /// text (that is [`ServerSelection::Text`]).
    Data { mime_type: String, data: Arc<Vec<u8>> },
}

/// Whether `client` is the shell the compositor launched.
fn privileged(client: &smithay::reexports::wayland_server::Client) -> bool {
    client.get_data::<ClientState>().is_some_and(|c| c.privileged)
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    /// The shell launched by the compositor (may use private protocols).
    pub privileged: bool,
    /// Its process id and command, for the log (set when it connects, or
    /// for the shell once it is started).
    pub identity: std::sync::OnceLock<(u32, String)>,
    /// The shell's socket (a second descriptor of it), to see how much it
    /// has not read yet; closed when it disconnects (so the connection
    /// really ends however long this state is kept).
    pub socket: std::sync::Mutex<Option<std::os::fd::OwnedFd>>,
    /// It runs in an app sandbox (Flatpak, snap), as its process shows.
    pub sandboxed: bool,
}

impl ClientState {
    /// A client that connected on `stream`: who it is.
    fn connected(stream: &std::os::unix::net::UnixStream) -> Self {
        use std::os::fd::AsFd;
        let mut state = Self::default();
        match helix_comp::client_socket::peer_pid(stream.as_fd()) {
            Some(pid) => {
                let _ = state.identity.set((pid, helix_comp::client_socket::command_of(pid)));
                state.sandboxed = helix_comp::client_socket::sandboxed(pid);
            }
            // Who it is can't be told: as sandboxed.
            None => state.sandboxed = true,
        }
        state
    }

    /// Bytes queued for it that it has not read.
    pub fn unread(&self) -> Option<usize> {
        use std::os::fd::AsFd;
        let socket = self.socket.lock().unwrap_or_else(|e| e.into_inner());
        socket.as_ref().and_then(|s| helix_comp::client_socket::unread_bytes(s.as_fd()))
    }
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, reason: DisconnectReason) {
        let (pid, command) = self.identity.get().cloned().unwrap_or_default();
        let unread = self.unread();
        drop(self.socket.lock().unwrap_or_else(|e| e.into_inner()).take());
        let why = helix_comp::client_socket::describe_disconnect(&reason, unread);
        if self.privileged {
            // The shell went: say why (it is restarted).
            match (&reason, unread) {
                (DisconnectReason::ConnectionClosed, Some(n)) if n > 0 => {
                    tracing::warn!(pid, command, reason = %why, "dropped the shell: it stopped reading its Wayland socket")
                }
                (DisconnectReason::ProtocolError(_), _) => tracing::warn!(pid, command, reason = %why, "dropped the shell for a protocol error"),
                _ => tracing::warn!(pid, command, reason = %why, "the shell's Wayland connection ended"),
            }
        } else {
            tracing::debug!(pid, command, reason = %why, "Wayland client disconnected");
        }
    }
}
