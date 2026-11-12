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
    // Before any other thread touches GIO.
    helix_settings::prepare_gio();

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
    let power_ui = ui_tx.clone();
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

    runtime.spawn(helix_shell::idle::run(service.connection().clone()));
    runtime.spawn(helix_shell::night_light::run(service.connection().clone(), service.night_light()));
    // The location from GeoClue (system bus) for night light and the
    // weather, when allowed.
    runtime.spawn(async {
        match zbus::Connection::system().await {
            Ok(system) => helix_shell::location::follow(system).await,
            Err(err) => tracing::info!(%err, "no system bus; night light keeps its stored location"),
        }
    });
    runtime.spawn(helix_shell::screencast_chooser::follow_sharing(service.connection().clone(), service.ui_sender()));
    runtime.spawn(helix_shell::notifications::follow_settings(service.notifications()));
    // High contrast for Helix's own UI.
    let contrast_ui = service.ui_sender();
    runtime.spawn(async move {
        const SCHEMA: &str = "org.gnome.desktop.a11y.interface";
        let read = || {
            use gio::prelude::*;
            gio::SettingsSchemaSource::default().and_then(|s| s.lookup(SCHEMA, true)).is_some_and(|_| gio::Settings::new(SCHEMA).boolean("high-contrast"))
        };
        let watched = helix_settings::GSettings::new(&[SCHEMA]);
        let (mut changes, _watch) = watched.watch();
        loop {
            let _ = contrast_ui.send(helix_shell::UiEvent::HighContrast(read()));
            if changes.recv().await.is_none() {
                break;
            }
        }
    });
    // Network, battery, volume and radios for the panel.
    runtime.spawn(helix_shell::status::follow(service.ui_sender()));
    // Microphone and camera use, from PipeWire.
    helix_shell::privacy::run(helix_shell::status::changed);
    // Dimming, blanking, automatic suspend, the power button, the battery.
    runtime.spawn(helix_shell::power::run_idle(service.connection().clone()));
    runtime.spawn(helix_shell::power::run_logind_locks(service.connection().clone()));
    runtime.spawn(helix_shell::power::run_battery(service.notifications()));
    // Dialogs for Helix's portal backends.
    {
        let conn = service.connection().clone();
        let dialogs = helix_shell::portal_dialog::PortalDialog::new(service.ui_sender());
        if let Err(err) = runtime.block_on(conn.object_server().at(helix_shell::portal_dialog::PATH, dialogs)) {
            tracing::warn!(%err, "cannot serve the portal dialogs");
        }
    }
    // Break reminders and screen time.
    runtime.spawn(helix_shell::wellbeing::run(service.connection().clone(), service.notifications(), service.ui_sender()));
    // Automounting and new removable media.
    helix_shell::media::run(service.notifications(), service.ui_sender(), runtime.handle().clone());
    // The desktop and lock screen backgrounds.
    runtime.spawn(helix_shell::wallpaper::follow(service.ui_sender()));
    // BlueZ's pairing agent.
    if std::env::var_os("HELIX_NO_BLUETOOTH_AGENT").is_none() {
        let (ui, center) = (service.ui_sender(), service.notifications());
        runtime.spawn(async move {
            match zbus::Connection::system().await {
                Ok(system) => {
                    if let Err(err) = helix_shell::bluetooth_agent::run(system, ui, center).await {
                        tracing::info!(%err, "no Bluetooth pairing agent");
                    }
                }
                Err(err) => tracing::info!(%err, "no system bus; no Bluetooth pairing agent"),
            }
        });
    }
    // NetworkManager's secret agent (Wi-Fi passwords).
    if std::env::var_os("HELIX_NO_NETWORK_AGENT").is_none() {
        let ui = service.ui_sender();
        runtime.spawn(async move {
            match zbus::Connection::system().await {
                Ok(system) => {
                    if let Err(err) = helix_shell::network_agent::run(system, ui).await {
                        tracing::info!(%err, "no NetworkManager secret agent");
                    }
                }
                Err(err) => tracing::info!(%err, "no system bus; no NetworkManager secret agent"),
            }
        });
    }
    // GeoClue's location agent.
    if std::env::var_os("HELIX_NO_GEOCLUE_AGENT").is_none() {
        let (ui, session) = (service.ui_sender(), service.connection().clone());
        runtime.spawn(async move {
            match zbus::Connection::system().await {
                Ok(system) => {
                    if let Err(err) = helix_shell::geoclue_agent::run(system, session, ui).await {
                        tracing::info!(%err, "no GeoClue agent");
                    }
                }
                Err(err) => tracing::info!(%err, "no system bus; no GeoClue agent"),
            }
        });
    }
    // The polkit authentication agent for our session.
    if std::env::var_os("HELIX_NO_POLKIT_AGENT").is_none() {
        let ui = service.ui_sender();
        runtime.spawn(async move {
            match helix_shell::polkit::register(ui).await {
                // Registered while this connection lives.
                Ok(_system) => std::future::pending::<()>().await,
                Err(err) => tracing::info!(%err, "no polkit authentication agent"),
            }
        });
    }
    // Changed shortcuts (Settings, custom shortcuts) apply at once.
    let rebind = service.ui_sender();
    helix_shell::extra_keys::follow(helix_shell::shell_keys::KEY_SCHEMAS, move || {
        let _ = rebind.send(helix_shell::UiEvent::RebindKeys);
    });

    // logind: which power actions to offer, and locking before sleep.
    let shell_conn = service.connection().clone();
    runtime.spawn(async move {
        let system = match zbus::Connection::system().await {
            Ok(conn) => conn,
            Err(err) => {
                tracing::warn!(%err, "no system bus; no suspend, restart or power off in the menu");
                return;
            }
        };
        let power = helix_shell::logind::availability(&system).await;
        tracing::info!(?power, "power actions");
        let _ = power_ui.send(helix_shell::UiEvent::PowerAvailability(power));
        helix_shell::logind::lock_before_sleep(system, move || {
            let conn = shell_conn.clone();
            async move {
                if helix_shell::idle::lock_on_suspend() {
                    helix_shell::lock_and_wait(&conn, std::time::Duration::from_secs(3)).await;
                }
            }
        })
        .await;
    });

    // Restarted by the compositor while the session was locked: lock again
    // at once (the compositor keeps the screen blank until we do).
    if std::env::var_os("HELIX_LOCKED").is_some() {
        tracing::info!("session is locked; showing the lock screen");
        if let Err(err) = runtime.block_on(service.lock()) {
            tracing::error!(%err, "cannot relock");
        }
    }

    let panel_result = if std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("WAYLAND_SOCKET").is_some() {
        ui::run(ui_channel, request_tx, service.authenticator())
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
