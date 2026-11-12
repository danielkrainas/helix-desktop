//! The location from GeoClue, while location services are on, for Night
//! Light's automatic schedule (helix-desktop-spec
//! `contracts/gsettings/night-light.md`: into
//! `night-light-last-coordinates`) and the weather's automatic location
//! (`contracts/gsettings/calendar.md`, "Weather").

use std::future::Future;

use futures_util::StreamExt;
use gio::prelude::*;
use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

pub const LOCATION: &str = "org.gnome.system.location";
const GEOCLUE: &str = "org.freedesktop.GeoClue2";
const MANAGER_PATH: &str = "/org/freedesktop/GeoClue2/Manager";
const CLIENT: &str = "org.freedesktop.GeoClue2.Client";
/// GeoClue's accuracy levels.
const COUNTRY: u32 = 1;
const CITY: u32 = 4;
/// The id GeoClue knows the shell by (a system component).
pub const DESKTOP_ID: &str = "org.gnome.Shell";

/// The accuracy to ask for: city, or coarser when `max-accuracy-level` is.
pub fn accuracy(max_level: &str) -> u32 {
    if max_level == "country" {
        COUNTRY
    } else {
        CITY
    }
}

/// Follow GeoClue on `conn`: every location it reports goes to `sink`,
/// until `stop` resolves.
pub async fn track(conn: &Connection, accuracy: u32, mut sink: impl FnMut(f64, f64), stop: impl Future<Output = ()>) -> zbus::Result<()> {
    let manager = zbus::Proxy::new(conn, GEOCLUE, MANAGER_PATH, "org.freedesktop.GeoClue2.Manager").await?;
    let client: OwnedObjectPath = manager.call("GetClient", &()).await?;
    let proxy = zbus::Proxy::new(conn, GEOCLUE, client.clone(), CLIENT).await?;
    proxy.set_property("DesktopId", DESKTOP_ID).await.map_err(zbus::Error::from)?;
    proxy.set_property("RequestedAccuracyLevel", accuracy).await.map_err(zbus::Error::from)?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(CLIENT)?
        .member("LocationUpdated")?
        .path(client.clone())?
        .build();
    let mut updates = zbus::MessageStream::for_match_rule(rule, conn, None).await?;
    proxy.call_method("Start", &()).await?;
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            update = updates.next() => {
                let Some(Ok(msg)) = update else { break };
                let Ok((_, location)) = msg.body().deserialize::<(OwnedObjectPath, OwnedObjectPath)>() else { continue };
                let place = zbus::Proxy::new(conn, GEOCLUE, location, "org.freedesktop.GeoClue2.Location").await?;
                let (lat, lon): (f64, f64) = (place.get_property("Latitude").await?, place.get_property("Longitude").await?);
                sink(lat, lon);
            }
        }
    }
    let _ = proxy.call_method("Stop", &()).await;
    Ok(())
}

/// The last location GeoClue reported while the weather wanted it.
static CURRENT: std::sync::Mutex<Option<(f64, f64)>> = std::sync::Mutex::new(None);

/// Where the user is, if known (only while the weather follows it).
pub fn current() -> Option<(f64, f64)> {
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner())
}

const WEATHER: &str = "org.gnome.shell.weather";

/// Who wants the location: (night light, weather).
fn wanting() -> (bool, bool) {
    let night = crate::night_light::read();
    let weather = settings(WEATHER, "automatic-location").is_some_and(|s| s.boolean("automatic-location"));
    (night.enabled && night.automatic, weather)
}

fn settings(schema: &str, key: &str) -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    source.lookup(schema, true)?.has_key(key).then(|| gio::Settings::new(schema))
}

/// Whether to follow GeoClue now, and at what accuracy.
fn wanted() -> Option<u32> {
    let (night, weather) = wanting();
    let on = settings(LOCATION, "enabled").is_some_and(|s| s.boolean("enabled"));
    let max = settings(LOCATION, "max-accuracy-level").map(|s| s.string("max-accuracy-level").to_string()).unwrap_or_default();
    ((night || weather) && on).then(|| accuracy(&max))
}

/// Follow the location while wanted (see the module notes); `system` is
/// the system bus.
pub async fn follow(system: Connection) {
    let watched = helix_settings::GSettings::new(&[crate::night_light::SCHEMA, LOCATION, WEATHER]);
    let (mut changes, _watch) = watched.watch();
    loop {
        *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = None;
        match wanted() {
            Some(level) => {
                let (night, weather) = wanting();
                tracing::info!(level, night, weather, "following the location");
                let stop = async {
                    changes.recv().await;
                };
                let store = |lat: f64, lon: f64| {
                    if weather {
                        *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = Some((lat, lon));
                    }
                    if !night {
                        return;
                    }
                    if let Some(s) = settings(crate::night_light::SCHEMA, "night-light-last-coordinates") {
                        let _ = s.set_value("night-light-last-coordinates", &(lat, lon).to_variant());
                        gio::Settings::sync();
                    }
                };
                if let Err(err) = track(&system, level, store, stop).await {
                    tracing::info!(%err, "no location from GeoClue");
                    // Wait for a settings change before trying again.
                    if changes.recv().await.is_none() {
                        return;
                    }
                }
            }
            None => {
                if changes.recv().await.is_none() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accuracy_levels() {
        assert_eq!(accuracy("exact"), CITY);
        assert_eq!(accuracy("city"), CITY);
        assert_eq!(accuracy("country"), COUNTRY);
    }
}
