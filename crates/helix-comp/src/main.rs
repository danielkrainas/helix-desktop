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

use helix_comp::display_config::Monitor;
use helix_comp::{CompositorServices, Event};
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

    let socket = state.socket_name.to_string_lossy().into_owned();
    tracing::info!(socket, "listening for Wayland clients");
    // SAFETY: still single-threaded here; children inherit the variable.
    std::env::set_var("WAYLAND_DISPLAY", &socket);

    start_dbus_thread(socket, monitors, events_rx);

    stop_on_signals(state.loop_signal.clone());

    if let Some(program) = shell {
        state.supervise_shell(program)?;
    }

    if let Some(program) = command {
        if let Err(err) = std::process::Command::new(&program).spawn() {
            tracing::warn!(%err, program, "failed to start client");
        }
    }

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
fn start_dbus_thread(
    socket: String,
    monitors: Vec<Monitor>,
    events: mpsc::UnboundedReceiver<Event>,
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
                let services = match zbus::connection::Builder::session() {
                    Ok(builder) => CompositorServices::start(builder, monitors).await,
                    Err(err) => Err(err),
                };
                let services = match services {
                    Ok(s) => s,
                    Err(err) => {
                        tracing::warn!(%err, "D-Bus services unavailable");
                        return;
                    }
                };
                announce_socket(services.connection(), &socket).await;
                services.run(events).await;
            });
        })
        .expect("spawn D-Bus thread");
}

/// Tell the session manager (if any) where clients should connect. This is
/// also the launcher's readiness signal for the compositor.
async fn announce_socket(conn: &zbus::Connection, socket: &str) {
    let result = async {
        let sm = SessionManagerProxy::new(conn).await?;
        sm.setenv("WAYLAND_DISPLAY", socket).await
    }
    .await;
    match result {
        Ok(()) => tracing::info!(socket, "announced WAYLAND_DISPLAY to the session manager"),
        Err(err) => tracing::info!(%err, "no session manager to announce WAYLAND_DISPLAY to"),
    }
}
