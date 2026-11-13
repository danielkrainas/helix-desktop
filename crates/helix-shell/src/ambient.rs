//! Brightness from the ambient light sensor (helix-desktop-spec
//! `contracts/gsettings/power.md`, "Ambient light"): with
//! `org.gnome.settings-daemon.plugins.power ambient-enabled`, the screen
//! follows the light iio-sensor-proxy measures.
//!
//! Only on bare hardware (`HELIX_BACKEND=drm`), where the sensor and the
//! backlight are this machine's. `HELIX_TEST_AMBIENT=1` looks for the
//! sensor on the session bus instead (tests with a fake).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use zbus::Connection;

const POWER: &str = "org.gnome.settings-daemon.plugins.power";
const SENSOR: &str = "net.hadess.SensorProxy";
const SENSOR_PATH: &str = "/net/hadess/SensorProxy";
/// The brightest light the curve tells apart (lux).
const BRIGHT_LUX: f64 = 2000.0;
/// The darkest automatic brightness (percent).
const FLOOR: f64 = 10.0;
/// How often brightness moves towards its target.
const TICK: Duration = Duration::from_millis(500);

/// The screen is dimmed for idleness (`power`): hands off.
pub static DIMMED: AtomicBool = AtomicBool::new(false);

/// The brightness (percent) for light `level` in `unit` (`lux`, or
/// `vendor`: 0–100 of the sensor's range), before the user's offset.
pub fn curve(level: f64, unit: &str) -> f64 {
    let fraction = if unit == "lux" { (1.0 + level.max(0.0)).log10() / (1.0 + BRIGHT_LUX).log10() } else { level / 100.0 };
    FLOOR + (100.0 - FLOOR) * fraction.clamp(0.0, 1.0)
}

/// The target with the user's `offset` (percent), kept within 1–100.
pub fn target(level: f64, unit: &str, offset: f64) -> u32 {
    (curve(level, unit) + offset).round().clamp(1.0, 100.0) as u32
}

/// One step from `current` towards `target`: a third of the way, at least
/// 1 percent; `None` within 2 percent (no flicker for small changes).
pub fn step(current: u32, target: u32) -> Option<u32> {
    let diff = i64::from(target) - i64::from(current);
    if diff.abs() <= 2 {
        return None;
    }
    let by = (diff.abs() / 3).max(1) * diff.signum();
    Some((i64::from(current) + by).clamp(0, 100) as u32)
}

fn enabled() -> bool {
    use gio::prelude::*;
    helix_settings::has_schema(POWER) && {
        let s = gio::Settings::new(POWER);
        s.settings_schema().is_some_and(|schema| schema.has_key("ambient-enabled")) && s.boolean("ambient-enabled")
    }
}

/// The bus the sensor is looked for on, when this may run.
async fn sensor_bus() -> Option<Connection> {
    if std::env::var("HELIX_TEST_AMBIENT").is_ok_and(|v| v == "1") {
        return Connection::session().await.ok();
    }
    if std::env::var("HELIX_BACKEND").is_ok_and(|b| b == "drm") {
        return Connection::system().await.ok();
    }
    None
}

/// Follows the light while `ambient-enabled` is on (`conn`: the session
/// bus, where DisplayConfig is).
pub async fn run(conn: Connection) {
    let Some(sensors) = sensor_bus().await else { return };
    if !helix_settings::has_schema(POWER) {
        return;
    }
    let watched = helix_settings::GSettings::new(&[POWER]);
    let (mut changes, _watch) = watched.watch();
    loop {
        if enabled() {
            tokio::select! {
                result = follow(&conn, &sensors) => {
                    if let Err(err) = result {
                        tracing::info!(%err, "no ambient light sensor");
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
            if !enabled() {
                let _ = sensors.call_method(Some(SENSOR), SENSOR_PATH, Some(SENSOR), "ReleaseLight", &()).await;
                tracing::info!("ambient light off");
            }
        } else if changes.recv().await.is_none() {
            return;
        }
    }
}

pub async fn follow(conn: &Connection, sensors: &Connection) -> zbus::Result<()> {
    let proxy = zbus::Proxy::new(sensors, SENSOR, SENSOR_PATH, SENSOR).await?;
    if !proxy.get_property::<bool>("HasAmbientLight").await.unwrap_or(false) {
        tracing::info!("no ambient light sensor");
        return std::future::pending().await;
    }
    proxy.call_method("ClaimLight", &()).await?;
    tracing::info!("following the ambient light");
    let mut levels = proxy.receive_property_changed::<f64>("LightLevel").await;
    let unit = proxy.get_property::<String>("LightLevelUnit").await.unwrap_or_else(|_| "lux".into());
    let mut level = proxy.get_property::<f64>("LightLevel").await.unwrap_or(0.0);
    // The user's correction, and what this last set (to tell their changes).
    let mut offset = 0.0;
    let mut last_set: Option<u32> = None;
    let mut tick = tokio::time::interval(TICK);
    loop {
        tokio::select! {
            change = levels.next() => {
                let Some(change) = change else { return Ok(()) };
                if let Ok(now) = change.get().await {
                    level = now;
                }
            }
            _ = tick.tick() => {
                if DIMMED.load(Ordering::SeqCst) {
                    last_set = None;
                    continue;
                }
                let Some(current) = crate::shell_keys::brightness_percent(conn).await else { continue };
                // Changed by someone else (the keys, Settings): their
                // choice shifts the curve.
                if last_set.is_some_and(|set| set.abs_diff(current) > 1) {
                    offset = f64::from(current) - curve(level, &unit);
                    tracing::info!(offset, "ambient brightness corrected by the user");
                }
                last_set = Some(current);
                if let Some(next) = step(current, target(level, &unit, offset)) {
                    tracing::debug!(level, current, next, "ambient brightness");
                    crate::shell_keys::set_brightness_percent(conn, next).await;
                    last_set = crate::shell_keys::brightness_percent(conn).await.or(Some(next));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_curve() {
        assert_eq!(target(0.0, "lux", 0.0), 10);
        assert_eq!(target(2000.0, "lux", 0.0), 100);
        assert_eq!(target(100_000.0, "lux", 0.0), 100);
        let room = target(300.0, "lux", 0.0);
        assert!((70..=80).contains(&room), "an office: {room}");
        assert_eq!(target(50.0, "vendor", 0.0), 55);
        assert_eq!(target(0.0, "lux", -20.0), 1, "never off");
        assert_eq!(target(300.0, "lux", 10.0), room + 10);
    }

    #[test]
    fn steps() {
        assert_eq!(step(50, 51), None);
        assert_eq!(step(50, 52), None);
        assert_eq!(step(50, 53), Some(51));
        assert_eq!(step(10, 100), Some(40));
        assert_eq!(step(100, 10), Some(70));
    }
}
