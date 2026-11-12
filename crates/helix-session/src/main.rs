//! `helix-session`: the Helix session leader.
//!
//! Owns `org.gnome.SessionManager` and, given a session config, starts the
//! session's components (see `helix_session::launcher`).
//!
//! ```text
//! helix-session [--config FILE]
//! ```
//!
//! Without `--config`, the config is looked up in
//! `$XDG_CONFIG_HOME/helix/session.toml` and `$XDG_CONFIG_DIRS`. With no
//! config at all, only the SessionManager service runs and a client must
//! call `Initialized` (handy for testing under `dbus-run-session`); logind
//! is then not used, so nothing can power off the machine.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use helix_session::launcher::{self, PowerSystem, SessionConfig};
use helix_session::logind::{self, Logind};
use helix_session::manager::SessionInfo;
use helix_session::{ServiceOptions, SessionEnd, SessionService};
use tokio::signal::unix::{signal, SignalKind};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    helix_session::locale::init_from_env();
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

fn config_path() -> Result<Option<PathBuf>, String> {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next(), args.next()) {
        (None, _, _) => Ok(SessionConfig::find()),
        (Some("--config"), Some(path), None) => Ok(Some(PathBuf::from(path))),
        _ => Err("usage: helix-session [--config FILE]".to_owned()),
    }
}

async fn run() -> Result<SessionEnd, Box<dyn std::error::Error>> {
    let config = match config_path()? {
        Some(path) => {
            tracing::info!(path = %path.display(), "using session config");
            Some(SessionConfig::load(&path)?)
        }
        None => {
            tracing::info!("no session config; serving SessionManager only");
            None
        }
    };

    let mut info = SessionInfo::default();
    if let Some(config) = &config {
        info.name = config.session.name.clone();
    }
    let logind = match config.as_ref().map(|c| c.power.system) {
        Some(PowerSystem::Logind) => Logind::system().await,
        Some(PowerSystem::None) | None => {
            tracing::info!("power actions disabled (no logind)");
            None
        }
    };
    let options = ServiceOptions {
        info,
        logind: logind.clone(),
        // Session restore only for a real session (with components).
        restore: config.as_ref().map(|_| helix_session::restore::Restore::default_path()),
        ..Default::default()
    };
    let service = SessionService::start_with_options(zbus::connection::Builder::session()?, options).await?;

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    let mut children = match &config {
        Some(config) => match launcher::launch(&service, config, &BTreeMap::new()).await {
            Ok(children) => {
                if config.session.autostart {
                    let apps = helix_session::autostart::Autostart::new(launcher::app_environment(&service, config, &BTreeMap::new()));
                    tokio::spawn(apps.run(std::future::pending()));
                }
                launcher::restore_apps(&service, config, &BTreeMap::new(), &service.apps_to_restore());
                Some(children)
            }
            Err(err) => {
                tracing::error!(%err, "session startup failed");
                service.run(std::future::ready(())).await?;
                return Err(err.into());
            }
        },
        None => None,
    };

    let stop = async {
        let required_exit = async {
            match children.as_mut() {
                Some(c) => {
                    c.required_exited().await;
                }
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("received SIGTERM"),
            _ = sigint.recv() => tracing::info!("received SIGINT"),
            () = required_exit => {},
        }
    };
    let conn = service.connection().clone();
    let end = service.run(stop).await?;
    helix_session::environment::retract(&conn).await;

    if let Some(children) = children {
        children.terminate(Duration::from_secs(5)).await;
    }
    if let SessionEnd::Requested(action) = end {
        logind::after_session(action, logind.as_ref()).await;
    }
    Ok(end)
}
