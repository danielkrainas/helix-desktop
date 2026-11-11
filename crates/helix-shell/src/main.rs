//! `helix-shell`: serves `org.gnome.Shell` and draws the panel and launcher.
//!
//! The D-Bus service runs on a tokio thread; the panel (a layer-shell client
//! of the compositor at `$WAYLAND_DISPLAY`) runs on the main thread. Without
//! a compositor, only the D-Bus service runs.

use std::process::ExitCode;

use helix_shell::{ui, ShellService};
use smithay_client_toolkit::reexports::calloop::channel;
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            tracing::error!(%err, "cannot start runtime");
            return ExitCode::FAILURE;
        }
    };
    let builder = match zbus::connection::Builder::session() {
        Ok(b) => b,
        Err(err) => {
            tracing::error!(%err, "no session bus");
            return ExitCode::FAILURE;
        }
    };
    let (service, mut ui_rx) = match runtime.block_on(ShellService::start(builder)) {
        Ok(started) => started,
        Err(err) => {
            tracing::error!(%err, "cannot serve org.gnome.Shell");
            return ExitCode::FAILURE;
        }
    };

    let (ui_tx, ui_channel) = channel::channel();
    runtime.spawn(async move {
        while let Some(event) = ui_rx.recv().await {
            if ui_tx.send(event).is_err() {
                break;
            }
        }
    });

    // The UI reports back: launcher open/close, key presses, failed grabs.
    let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
    let bridge = service.ui_bridge();
    runtime.spawn(async move {
        while let Some(request) = request_rx.recv().await {
            if let Err(err) = bridge.handle(request).await {
                tracing::warn!(%err, "cannot apply UI request");
            }
        }
    });

    let panel_result = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        ui::run(ui_channel, request_tx)
    } else {
        tracing::warn!("WAYLAND_DISPLAY not set; running without a panel");
        Err("no compositor".into())
    };
    if let Err(err) = panel_result {
        tracing::warn!(%err, "panel unavailable; serving org.gnome.Shell only");
        runtime.block_on(async {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        });
    }
    let _ = runtime.block_on(service.shutdown());
    ExitCode::SUCCESS
}
