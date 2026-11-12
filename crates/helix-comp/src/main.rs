//! `helix-comp`: the Helix compositor.
//!
//! ```text
//! helix-comp [--backend winit|drm] [--shell PROGRAM] [--command PROGRAM]
//! ```
//!
//! Backends:
//! - `winit`: nested in a window of an existing X11 or Wayland session
//!   (the default when `WAYLAND_DISPLAY` or `DISPLAY` is set);
//! - `drm`: on bare hardware from a text console, via libseat, KMS and
//!   libinput (the default otherwise). Ctrl+Alt+Backspace quits;
//!   Ctrl+Alt+F1..F12 switch VTs.
//!
//! `--shell` starts the shell as the compositor's privileged client (on a
//! pre-connected socket), which is what lets it bind global keys through
//! the private `helix_keybindings_v1` protocol.
//!
//! Serves `org.gnome.Mutter.IdleMonitor` and `org.gnome.Mutter.DisplayConfig`
//! on the session bus and announces its Wayland socket to the session
//! manager with `Setenv("WAYLAND_DISPLAY", ...)`.

mod compositor;

use std::process::ExitCode;

use helix_comp::display_config::{ApplyRequest, ColorRequest, Monitor, Store};
use helix_comp::screencast::CastRequest;
use helix_comp::{CompositorServices, Event, MonitorControl};
use helix_session::client::SessionManagerProxy;
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use compositor::Helix;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    // Before any other thread touches GIO.
    helix_settings::prepare_gio();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(%err, "helix-comp failed");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Winit,
    Drm,
}

struct Args {
    backend: Backend,
    shell: Option<String>,
    command: Option<String>,
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let Args { backend, shell, command } = parse_args()?;
    tracing::info!(?backend, "starting");

    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let mut event_loop: EventLoop<'static, Helix> = EventLoop::try_new()?;
    let display: Display<Helix> = Display::new()?;
    let mut state = Helix::new(&mut event_loop, display, events_tx)?;
    let monitors = match backend {
        Backend::Winit => compositor::winit::init(&mut event_loop, &mut state)?,
        Backend::Drm => compositor::udev::init(&mut event_loop, &mut state)?,
    };

    // Keep the shell's window list current (titles, focus, open/close).
    event_loop
        .handle()
        .insert_source(
            smithay::reexports::calloop::timer::Timer::immediate(),
            |_, _, state| {
                state.sync_window_list();
                state.update_surface_scales();
                smithay::reexports::calloop::timer::TimeoutAction::ToDuration(std::time::Duration::from_millis(200))
            },
        )
        .map_err(|e| format!("cannot start the window list timer: {e}"))?;

    // Focus following the pointer, auto-raise and animations.
    event_loop
        .handle()
        .insert_source(smithay::reexports::calloop::timer::Timer::immediate(), |_, _, state| {
            state.wm_tick();
            smithay::reexports::calloop::timer::TimeoutAction::ToDuration(std::time::Duration::from_millis(50))
        })
        .map_err(|e| format!("cannot start the window-management timer: {e}"))?;

    // Keyboard accessibility: slow keys, mouse keys, timeouts.
    event_loop
        .handle()
        .insert_source(smithay::reexports::calloop::timer::Timer::immediate(), |_, _, state| {
            state.a11y_tick();
            smithay::reexports::calloop::timer::TimeoutAction::ToDuration(std::time::Duration::from_millis(20))
        })
        .map_err(|e| format!("cannot start the accessibility timer: {e}"))?;

    // Keyboard, mouse and touchpad settings: now, and whenever they change.
    state.apply_a11y_settings();
    state.apply_magnifier_settings();
    state.apply_input_settings();
    state.apply_workspace_settings();
    state.apply_frame_settings();
    state.apply_wm_settings();
    let (settings_tx, settings_rx) = smithay::reexports::calloop::channel::channel::<()>();
    event_loop
        .handle()
        .insert_source(settings_rx, |event, _, state| {
            if let smithay::reexports::calloop::channel::Event::Msg(()) = event {
                state.apply_a11y_settings();
                state.apply_magnifier_settings();
                state.apply_input_settings();
                state.apply_workspace_settings();
                state.apply_frame_settings();
                state.apply_wm_settings();
                state.reread_x11_settings();
            }
        })
        .map_err(|e| format!("cannot follow input settings: {e}"))?;
    std::thread::Builder::new()
        .name("helix-comp-settings".into())
        .spawn(move || {
            let mut schemas: Vec<&str> = helix_comp::input_settings::SCHEMAS
                .iter()
                .chain(helix_comp::workspaces::SCHEMAS.iter())
                .chain(helix_comp::frames::SCHEMAS.iter())
                .chain(helix_comp::wm_settings::SCHEMAS.iter())
                .chain(helix_comp::a11y_keys::SCHEMAS.iter())
                .chain(helix_comp::a11y_mouse::SCHEMAS.iter())
                .chain(helix_comp::magnifier::SCHEMAS.iter())
                .chain(helix_comp::xsettings::SCHEMAS.iter())
                .copied()
                .collect();
            schemas.sort_unstable();
            schemas.dedup();
            let settings = helix_settings::GSettings::new(&schemas);
            let (mut changes, _watch) = settings.watch();
            while changes.blocking_recv().is_some() {
                if settings_tx.send(()).is_err() {
                    break;
                }
            }
        })?;

    // ApplyMonitorsConfig requests come from the D-Bus thread.
    let (apply_tx, apply_rx) = smithay::reexports::calloop::channel::channel::<ApplyRequest>();
    event_loop
        .handle()
        .insert_source(apply_rx, |event, _, state| {
            if let smithay::reexports::calloop::channel::Event::Msg(request) = event {
                let result = state.apply_monitors(&request.config);
                if let Err(err) = &result {
                    tracing::warn!(%err, "cannot apply the monitor configuration");
                }
                let _ = request.reply.send(result);
            }
        })
        .map_err(|e| format!("cannot listen for monitor changes: {e}"))?;

    // Screen cast requests, also from the D-Bus thread.
    let (cast_tx, cast_rx) = smithay::reexports::calloop::channel::channel::<CastRequest>();
    event_loop
        .handle()
        .insert_source(cast_rx, |event, _, state| {
            if let smithay::reexports::calloop::channel::Event::Msg(request) = event {
                state.cast_request(request);
            }
        })
        .map_err(|e| format!("cannot listen for screen cast requests: {e}"))?;

    // GetCrtcGamma / SetCrtcGamma, also from the D-Bus thread.
    let (gamma_tx, gamma_rx) = smithay::reexports::calloop::channel::channel::<ColorRequest>();
    event_loop
        .handle()
        .insert_source(gamma_rx, |event, _, state| {
            if let smithay::reexports::calloop::channel::Event::Msg(request) = event {
                state.gamma_request(request);
            }
        })
        .map_err(|e| format!("cannot listen for gamma changes: {e}"))?;

    let socket = state.socket_name.to_string_lossy().into_owned();
    tracing::info!(socket, "listening for Wayland clients");
    // SAFETY: still single-threaded here; children inherit the variable.
    std::env::set_var("WAYLAND_DISPLAY", &socket);

    // Xwayland comes up first; then DISPLAY and WAYLAND_DISPLAY are
    // announced (in that order) and the shell and --command start.
    let (xdisplay_tx, xdisplay_rx) = tokio::sync::oneshot::channel();
    // Where the magnifier's focus and caret are, from the D-Bus thread.
    let (track_tx, track_rx) = smithay::reexports::calloop::channel::channel::<helix_comp::focus_tracking::TrackPoint>();
    event_loop
        .handle()
        .insert_source(track_rx, |event, _, state| {
            if let smithay::reexports::calloop::channel::Event::Msg(point) = event {
                state.track_point(point);
            }
        })
        .map_err(|e| format!("cannot follow focus for the magnifier: {e}"))?;
    start_dbus_thread(socket, monitors, matches!(backend, Backend::Drm), events_rx, xdisplay_rx, apply_tx, gamma_tx, cast_tx, track_tx);

    stop_on_signals(state.loop_signal.clone());

    state.start_xwayland(move |state, xdisplay| {
        let _ = xdisplay_tx.send(xdisplay);
        if let Some(program) = shell {
            if let Err(err) = state.supervise_shell(program) {
                tracing::error!(%err, "cannot supervise the shell");
            }
        }
        if let Some(program) = command {
            let mut cmd = std::process::Command::new(&program);
            state.set_child_display(&mut cmd);
            if let Err(err) = cmd.spawn() {
                tracing::warn!(%err, program, "failed to start client");
            }
        }
    });

    let result = event_loop.run(None, &mut state, |_| {});
    state.stop_shell();
    result?;
    Ok(())
}

/// SIGTERM (as sent by the session launcher) or SIGINT end the event loop,
/// so shutdown runs normally (including stopping the shell).
fn stop_on_signals(signal: smithay::reexports::calloop::LoopSignal) {
    std::thread::Builder::new()
        .name("helix-comp-signals".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
                return;
            };
            runtime.block_on(async {
                use tokio::signal::unix::{signal as unix_signal, SignalKind};
                let (Ok(mut term), Ok(mut int)) =
                    (unix_signal(SignalKind::terminate()), unix_signal(SignalKind::interrupt()))
                else {
                    return;
                };
                tokio::select! {
                    _ = term.recv() => tracing::info!("received SIGTERM"),
                    _ = int.recv() => tracing::info!("received SIGINT"),
                }
            });
            signal.stop();
            signal.wakeup();
        })
        .expect("spawn signal thread");
}

fn parse_args() -> Result<Args, String> {
    const USAGE: &str = "usage: helix-comp [--backend winit|drm] [--shell PROGRAM] [--command PROGRAM]";
    let nested = std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
    let mut parsed = Args {
        backend: if nested { Backend::Winit } else { Backend::Drm },
        shell: None,
        command: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--backend" => {
                parsed.backend = match args.next().as_deref() {
                    Some("winit") => Backend::Winit,
                    Some("drm") => Backend::Drm,
                    _ => return Err(USAGE.into()),
                }
            }
            "--command" | "-c" => parsed.command = Some(args.next().ok_or(USAGE)?),
            "--shell" => parsed.shell = Some(args.next().ok_or(USAGE)?),
            _ => return Err(USAGE.into()),
        }
    }
    Ok(parsed)
}

/// D-Bus services live on their own thread with a tokio runtime. Failure to
/// reach the bus is not fatal: the compositor still works without them.
#[allow(clippy::too_many_arguments)]
fn start_dbus_thread(
    socket: String,
    monitors: Vec<Monitor>,
    // Real monitors: control external ones over DDC/CI.
    ddc: bool,
    events: mpsc::UnboundedReceiver<Event>,
    xdisplay: tokio::sync::oneshot::Receiver<Option<u32>>,
    to_compositor: smithay::reexports::calloop::channel::Sender<ApplyRequest>,
    gamma_to_compositor: smithay::reexports::calloop::channel::Sender<ColorRequest>,
    cast_to_compositor: smithay::reexports::calloop::channel::Sender<CastRequest>,
    track_to_compositor: smithay::reexports::calloop::channel::Sender<helix_comp::focus_tracking::TrackPoint>,
) {
    std::thread::Builder::new()
        .name("helix-comp-dbus".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    tracing::error!(%err, "cannot start D-Bus runtime");
                    return;
                }
            };
            runtime.block_on(async move {
                let (applier, mut requests) = mpsc::unbounded_channel::<ApplyRequest>();
                tokio::spawn(async move {
                    while let Some(request) = requests.recv().await {
                        if to_compositor.send(request).is_err() {
                            break;
                        }
                    }
                });
                let (gamma, mut gamma_requests) = mpsc::unbounded_channel::<ColorRequest>();
                tokio::spawn(async move {
                    while let Some(request) = gamma_requests.recv().await {
                        if gamma_to_compositor.send(request).is_err() {
                            break;
                        }
                    }
                });
                let (cast, mut cast_requests) = mpsc::unbounded_channel::<CastRequest>();
                tokio::spawn(async move {
                    while let Some(request) = cast_requests.recv().await {
                        if cast_to_compositor.send(request).is_err() {
                            break;
                        }
                    }
                });
                let control = MonitorControl {
                    applier: Some(applier),
                    gamma: Some(gamma),
                    cast: Some(cast),
                    store: Store::user(),
                    backlight: Some(helix_comp::backlight::Backlight::system(ddc).await),
                    ..MonitorControl::default()
                };
                let services = match zbus::connection::Builder::session() {
                    Ok(builder) => CompositorServices::start_with(builder, monitors, control).await,
                    Err(err) => Err(err),
                };
                let services = match services {
                    Ok(s) => s,
                    Err(err) => {
                        tracing::warn!(%err, "D-Bus services unavailable");
                        return;
                    }
                };
                // Wait for Xwayland (or its failure) so DISPLAY is set
                // before the launcher sees WAYLAND_DISPLAY and moves on.
                let xdisplay = tokio::time::timeout(std::time::Duration::from_secs(10), xdisplay)
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .flatten();
                announce_socket(services.connection(), &socket, xdisplay).await;
                // Focus tracking is switched here; other events go on.
                let (want_tx, want_rx) = tokio::sync::watch::channel(false);
                let (forward, forwarded) = mpsc::unbounded_channel();
                let mut events = events;
                tokio::spawn(async move {
                    while let Some(event) = events.recv().await {
                        match event {
                            Event::FocusTracking(on) => {
                                let _ = want_tx.send(on);
                            }
                            other => {
                                if forward.send(other).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
                tokio::spawn(helix_comp::focus_tracking::run(services.connection().clone(), want_rx, track_to_compositor));
                services.run(forwarded).await;
            });
        })
        .expect("spawn D-Bus thread");
}

/// Tell the session manager (if any) where clients should connect. This is
/// also the launcher's readiness signal for the compositor.
async fn announce_socket(conn: &zbus::Connection, socket: &str, xdisplay: Option<u32>) {
    let result = async {
        let sm = SessionManagerProxy::new(conn).await?;
        if let Some(n) = xdisplay {
            sm.setenv("DISPLAY", &format!(":{n}")).await?;
        }
        sm.setenv("WAYLAND_DISPLAY", socket).await
    }
    .await;
    match result {
        Ok(()) => tracing::info!(socket, "announced WAYLAND_DISPLAY to the session manager"),
        Err(err) => tracing::info!(%err, "no session manager to announce WAYLAND_DISPLAY to"),
    }
}
