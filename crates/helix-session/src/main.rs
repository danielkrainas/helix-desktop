//! `helix-session`: owns `org.gnome.SessionManager` on the session bus.
//!
//! Run under a throwaway bus with:
//!
//! ```sh
//! dbus-run-session -- cargo run -p helix-session
//! ```

use std::process::ExitCode;

use helix_session::{SessionEnd, SessionService};
use tokio::signal::unix::{signal, SignalKind};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    match run().await {
        Ok(end) => {
            tracing::info!(?end, "session over");
            ExitCode::SUCCESS
        }
        Err(err) => {
            tracing::error!(%err, "helix-session failed");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<SessionEnd, Box<dyn std::error::Error>> {
    let builder = zbus::connection::Builder::session()?;
    let service = SessionService::start(builder).await?;

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let stop = async move {
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("received SIGTERM"),
            _ = sigint.recv() => tracing::info!("received SIGINT"),
        }
    };

    Ok(service.run(stop).await?)
}
