//! `helix-portals`: Helix's XDG desktop portal backends.
//!
//! Started on demand by xdg-desktop-portal (see `session/helix.portal`) or by
//! the Helix session launcher.

use std::process::ExitCode;
use std::sync::Arc;

use helix_portals::PortalService;
use helix_settings::{GSettings, PORTAL_SCHEMAS};
use tokio::signal::unix::{signal, SignalKind};
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    // The locale and translations of the portals' own dialogs
    // (helix-desktop-spec components/i18n.md), before any thread starts.
    helix_i18n::init();
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
    match runtime.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(%err, "helix-portals failed");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    helix_settings::prepare_gio();
    let gsettings = GSettings::new(PORTAL_SCHEMAS);
    let (mut changes, _watch) = gsettings.watch();
    let (mut lockdown, _lockdown_watch) = GSettings::new(helix_portals::lockdown::SCHEMAS).watch();
    let mut service =
        PortalService::start(zbus::connection::Builder::session()?, Arc::new(gsettings)).await?;

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    loop {
        tokio::select! {
            Some(schema) = changes.recv() => {
                tracing::debug!(%schema, "settings changed");
                if let Err(err) = service.settings_changed().await {
                    tracing::warn!(%err, "failed to publish settings change");
                }
            }
            Some(_) = lockdown.recv() => {
                if let Err(err) = service.lockdown_changed().await {
                    tracing::warn!(%err, "failed to publish lockdown change");
                }
            }
            () = service.bus_closed() => {
                tracing::warn!("the session bus went away");
                break;
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }
    service.shutdown().await?;
    Ok(())
}
