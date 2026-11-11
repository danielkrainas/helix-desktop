//! `helix-comp`: the Helix compositor.
//!
//! ```text
//! helix-comp [--command PROGRAM]
//! ```
//!
//! Runs nested (winit backend) inside an existing X11 or Wayland session.
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

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let command = parse_args()?;

    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let mut event_loop: EventLoop<'static, Helix> = EventLoop::try_new()?;
    let display: Display<Helix> = Display::new()?;
    let mut state = Helix::new(&mut event_loop, display, events_tx)?;
    let monitors = compositor::winit::init(&mut event_loop, &mut state)?;

    let socket = state.socket_name.to_string_lossy().into_owned();
    tracing::info!(socket, "listening for Wayland clients");
    // SAFETY: still single-threaded here; children inherit the variable.
    std::env::set_var("WAYLAND_DISPLAY", &socket);

    start_dbus_thread(socket, monitors, events_rx);

    if let Some(program) = command {
        if let Err(err) = std::process::Command::new(&program).spawn() {
            tracing::warn!(%err, program, "failed to start client");
        }
    }

    event_loop.run(None, &mut state, |_| {})?;
    Ok(())
}

fn parse_args() -> Result<Option<String>, String> {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next(), args.next()) {
        (None, _, _) => Ok(None),
        (Some("--command" | "-c"), Some(program), None) => Ok(Some(program)),
        _ => Err("usage: helix-comp [--command PROGRAM]".into()),
    }
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
