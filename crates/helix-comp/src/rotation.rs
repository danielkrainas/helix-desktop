//! Automatic rotation of the built-in panel (helix-desktop-spec
//! `org.gnome.Mutter.DisplayConfig.md`, "Automatic rotation").

use futures_util::StreamExt;
use zbus::Connection;

use crate::display_config::{self, DisplayConfig, MonitorsConfig};

pub const LOCK_SCHEMA: &str = "org.gnome.settings-daemon.peripherals.touchscreen";
const SENSOR: &str = "net.hadess.SensorProxy";
const SENSOR_PATH: &str = "/net/hadess/SensorProxy";

/// The panel transform for an accelerometer orientation.
pub fn transform_of(orientation: &str) -> Option<u32> {
    match orientation {
        "normal" => Some(0),
        "left-up" => Some(1),
        "bottom-up" => Some(2),
        "right-up" => Some(3),
        _ => None,
    }
}

/// `config` with the built-in panel `builtin` turned to `transform`;
/// `None` when nothing changes.
pub fn rotated(mut config: MonitorsConfig, builtin: &str, transform: u32) -> Option<MonitorsConfig> {
    let logical = config.logical.iter_mut().find(|l| l.monitors.iter().any(|m| m.connector == builtin))?;
    if logical.transform == transform {
        return None;
    }
    logical.transform = transform;
    Some(config)
}

fn locked() -> bool {
    use gio::prelude::*;
    let Some(source) = gio::SettingsSchemaSource::default() else { return true };
    match source.lookup(LOCK_SCHEMA, true) {
        Some(schema) if schema.has_key("orientation-lock") => gio::Settings::new(LOCK_SCHEMA).boolean("orientation-lock"),
        // Without the setting there is no way to turn it off: don't rotate.
        _ => true,
    }
}

/// Turn the built-in panel as `orientation` says.
async fn apply(session: &Connection, orientation: &str) {
    let Some(transform) = transform_of(orientation) else { return };
    let Ok(iface) = session.object_server().interface::<_, DisplayConfig>(display_config::OBJECT_PATH).await else { return };
    let config = {
        let display = iface.get().await;
        let layout = display.layout();
        let Some(builtin) = layout.monitors.iter().find(|m| m.builtin && m.current_mode().is_some()).map(|m| m.connector.clone()) else { return };
        rotated(layout.current_config(), &builtin, transform)
    };
    let Some(config) = config else { return };
    tracing::info!(orientation, transform, "rotating the built-in panel");
    let result = iface.get().await.apply_and_publish(config, iface.signal_emitter()).await;
    if let Err(err) = result {
        tracing::warn!(err, "cannot rotate the built-in panel");
    }
}

/// Follow the accelerometer while rotation is unlocked.
async fn follow_sensor(session: &Connection, system: &Connection) -> zbus::Result<()> {
    let proxy = zbus::Proxy::new(system, SENSOR, SENSOR_PATH, SENSOR).await?;
    if !proxy.get_property::<bool>("HasAccelerometer").await.unwrap_or(false) {
        tracing::info!("no accelerometer; no automatic rotation");
        return std::future::pending().await;
    }
    let mut changes = proxy.receive_property_changed::<String>("AccelerometerOrientation").await;
    proxy.call_method("ClaimAccelerometer", &()).await?;
    if let Ok(now) = proxy.get_property::<String>("AccelerometerOrientation").await {
        apply(session, &now).await;
    }
    while let Some(change) = changes.next().await {
        if let Ok(orientation) = change.get().await {
            apply(session, &orientation).await;
        }
    }
    Ok(())
}

/// Rotate while `orientation-lock` is off (`session`: the connection
/// serving DisplayConfig).
pub async fn follow(session: Connection) {
    let Ok(system) = Connection::system().await else { return };
    run(session, system).await;
}

/// [`follow`] with a given system bus (tests).
pub async fn run(session: Connection, system: Connection) {
    let watched = helix_settings::GSettings::new(&[LOCK_SCHEMA]);
    let (mut changes, _watch) = watched.watch();
    loop {
        if locked() {
            if changes.recv().await.is_none() {
                return;
            }
            continue;
        }
        tracing::debug!("automatic rotation on");
        tokio::select! {
            result = follow_sensor(&session, &system) => {
                if let Err(err) = result {
                    tracing::info!(%err, "no automatic rotation");
                }
                if changes.recv().await.is_none() {
                    return;
                }
            }
            change = changes.recv() => {
                if change.is_none() {
                    return;
                }
            }
        }
        if locked() {
            let _ = system.call_method(Some(SENSOR), SENSOR_PATH, Some(SENSOR), "ReleaseAccelerometer", &()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_config::{LogicalConfig, MonitorChoice};

    #[test]
    fn orientations() {
        assert_eq!(transform_of("normal"), Some(0));
        assert_eq!(transform_of("left-up"), Some(1));
        assert_eq!(transform_of("bottom-up"), Some(2));
        assert_eq!(transform_of("right-up"), Some(3));
        assert_eq!(transform_of("undefined"), None);
    }

    #[test]
    fn only_the_panel_turns() {
        let monitor = |c: &str| MonitorChoice { connector: c.into(), width: 1920, height: 1080, refresh_mhz: 60000 };
        let config = MonitorsConfig {
            logical: vec![
                LogicalConfig { x: 0, y: 0, scale: 1.0, transform: 0, primary: true, monitors: vec![monitor("eDP-1")] },
                LogicalConfig { x: 1920, y: 0, scale: 1.0, transform: 0, primary: false, monitors: vec![monitor("HDMI-A-1")] },
            ],
        };
        let turned = rotated(config.clone(), "eDP-1", 1).unwrap();
        assert_eq!(turned.logical[0].transform, 1);
        assert_eq!(turned.logical[1].transform, 0);
        assert!(rotated(config.clone(), "eDP-1", 0).is_none(), "unchanged");
        assert!(rotated(config, "DSI-1", 1).is_none(), "no such panel");
    }
}
