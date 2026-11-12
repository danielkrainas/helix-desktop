//! What the message tray's calendar column shows (helix-desktop-spec
//! `contracts/gsettings/calendar.md`): events from Evolution Data Server,
//! world clocks, and the media player.

use std::collections::HashMap;
use std::time::Duration;

use gio::prelude::*;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::Connection;

use crate::ical::{self, Occurrence};
use crate::shell_keys::{MPRIS_PATH, MPRIS_PLAYER};

const SOURCES: &str = "org.gnome.evolution.dataserver.Sources5";
const SOURCES_PATH: &str = "/org/gnome/evolution/dataserver/SourceManager";
const FACTORY: &str = "org.gnome.evolution.dataserver.Calendar8";
const FACTORY_PATH: &str = "/org/gnome/evolution/dataserver/CalendarFactory";
const CALENDAR: &str = "org.gnome.evolution.dataserver.Calendar";
const WORLD_CLOCKS: &str = "org.gnome.shell.world-clocks";
const EDS_TIMEOUT: Duration = Duration::from_secs(10);

/// The media player's state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Player {
    pub identity: String,
    pub title: String,
    pub artist: String,
    pub playing: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CalendarData {
    /// The time range the events are for.
    pub from: i64,
    pub to: i64,
    pub events: Vec<Occurrence>,
    /// World clocks: (name, time zone).
    pub clocks: Vec<(String, String)>,
    pub player: Option<Player>,
    /// With a weather location: its forecast, if it could be had.
    pub weather: Option<Option<crate::weather::Forecast>>,
    /// How temperatures are shown.
    pub unit: Option<crate::weather::Unit>,
}

/// Everything for `[from, to)`.
pub async fn read(conn: &Connection, from: i64, to: i64) -> CalendarData {
    let events = match tokio::time::timeout(EDS_TIMEOUT, events(conn, from, to)).await {
        Ok(events) => events,
        Err(_) => {
            tracing::info!("calendars too slow");
            Vec::new()
        }
    };
    let clocks = tokio::task::spawn_blocking(world_clocks).await.unwrap_or_default();
    let (weather, unit) = tokio::task::spawn_blocking(|| (crate::weather::current(), crate::weather::unit())).await.unwrap_or((None, crate::weather::Unit::Celsius));
    CalendarData { from, to, events, clocks, player: player(conn).await, weather, unit: Some(unit) }
}

// ----- Events ----------------------------------------------------------

/// Whether a source's key file is an enabled, selected calendar.
pub fn is_shown_calendar(data: &str) -> bool {
    let (mut group, mut calendar, mut shown) = (String::new(), false, true);
    for line in data.lines().map(str::trim) {
        if let Some(g) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            group = g.to_owned();
            calendar |= group == "Calendar";
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let off = v.trim() == "false";
        if (group == "Data Source" && k.trim() == "Enabled" && off) || (group == "Calendar" && k.trim() == "Selected" && off) {
            shown = false;
        }
    }
    calendar && shown
}

fn time_text(unix: i64) -> String {
    gio::glib::DateTime::from_unix_utc(unix).and_then(|d| d.format("%Y%m%dT%H%M%SZ")).map(|s| s.to_string()).unwrap_or_default()
}

async fn events(conn: &Connection, from: i64, to: i64) -> Vec<Occurrence> {
    type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;
    let objects: Objects = match conn.call_method(Some(SOURCES), SOURCES_PATH, Some("org.freedesktop.DBus.ObjectManager"), "GetManagedObjects", &()).await {
        Ok(reply) => reply.body().deserialize().unwrap_or_default(),
        Err(err) => {
            tracing::debug!(%err, "no calendar sources");
            return Vec::new();
        }
    };
    let text = |v: Option<&OwnedValue>| v.and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
    let uids: Vec<String> = objects
        .values()
        .filter_map(|ifaces| ifaces.get("org.gnome.evolution.dataserver.Source"))
        .filter(|props| is_shown_calendar(&text(props.get("Data"))))
        .map(|props| text(props.get("UID")))
        .filter(|uid| !uid.is_empty())
        .collect();
    let query = format!("(occur-in-time-range? (make-time \"{}\") (make-time \"{}\"))", time_text(from), time_text(to));
    let mut out = Vec::new();
    for uid in uids {
        let opened = async {
            let reply = conn.call_method(Some(FACTORY), FACTORY_PATH, Some("org.gnome.evolution.dataserver.CalendarFactory"), "OpenCalendar", &(uid.as_str(),)).await?;
            let (path, bus): (String, String) = reply.body().deserialize()?;
            conn.call_method(Some(bus.as_str()), path.as_str(), Some(CALENDAR), "Open", &()).await?;
            let reply = conn.call_method(Some(bus.as_str()), path.as_str(), Some(CALENDAR), "GetObjectList", &(query.as_str(),)).await?;
            reply.body().deserialize::<Vec<String>>()
        };
        match opened.await {
            Ok(ics) => out.extend(ical::occurrences(&ics, from, to)),
            Err(err) => tracing::debug!(%err, uid, "cannot read calendar"),
        }
    }
    out.sort_by_key(|a| (!a.all_day, a.start));
    out
}

// ----- World clocks ----------------------------------------------------

/// A serialized GWeather location (version 2): name, latitude and
/// longitude in degrees (the city's, else the station's).
pub fn parse_location(v: &gio::glib::Variant) -> Option<(String, f64, f64)> {
    let inner = v.as_variant().unwrap_or_else(|| v.clone());
    let (version, data): (u32, gio::glib::Variant) = inner.get()?;
    if version != 2 {
        return None;
    }
    let data = data.as_variant().unwrap_or(data);
    let name: String = data.try_child_value(0)?.get()?;
    let coords = |i| -> Option<(f64, f64)> { data.try_child_value(i)?.get::<Vec<(f64, f64)>>()?.first().copied() };
    let (lat, lon) = coords(3).or_else(|| coords(4))?;
    Some((name, lat.to_degrees(), lon.to_degrees()))
}

/// ISO 6709 `±DDMM[SS]±DDDMM[SS]` → degrees.
fn parse_iso6709(s: &str) -> Option<(f64, f64)> {
    let split = s[1..].find(['+', '-'])? + 1;
    let part = |p: &str, deg_digits: usize| -> Option<f64> {
        let sign = if p.starts_with('-') { -1.0 } else { 1.0 };
        let digits = &p[1..];
        let d: f64 = digits.get(..deg_digits)?.parse().ok()?;
        let m: f64 = digits.get(deg_digits..deg_digits + 2)?.parse().ok()?;
        let sec: f64 = digits.get(deg_digits + 2..deg_digits + 4).and_then(|x| x.parse().ok()).unwrap_or(0.0);
        Some(sign * (d + m / 60.0 + sec / 3600.0))
    };
    Some((part(&s[..split], 2)?, part(&s[split..], 3)?))
}

/// The zone of `zone1970.tab` whose principal city is nearest, and how far
/// that city is (km).
fn nearest(tab: &str, lat: f64, lon: f64) -> Option<(f64, String)> {
    let (lat, lon) = (lat.to_radians(), lon.to_radians());
    tab.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let mut cols = l.split('\t');
            let (_, coord, zone) = (cols.next()?, cols.next()?, cols.next()?);
            let (zlat, zlon) = parse_iso6709(coord)?;
            let (zlat, zlon) = (zlat.to_radians(), zlon.to_radians());
            // Great-circle distance (haversine).
            let a = ((zlat - lat) / 2.0).sin().powi(2) + lat.cos() * zlat.cos() * ((zlon - lon) / 2.0).sin().powi(2);
            Some((2.0 * 6371.0 * a.sqrt().min(1.0).asin(), zone.to_owned()))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
}

/// The zone of `zone1970.tab` whose principal city is nearest.
pub fn nearest_zone(tab: &str, lat: f64, lon: f64) -> Option<String> {
    nearest(tab, lat, lon).map(|(_, zone)| zone)
}

/// The name of the `zone1970.tab` principal city within `max_km`, e.g.
/// "New York" for `America/New_York`.
pub fn nearby_city(tab: &str, lat: f64, lon: f64, max_km: f64) -> Option<String> {
    let (km, zone) = nearest(tab, lat, lon)?;
    (km <= max_km).then(|| zone.rsplit('/').next().unwrap_or(&zone).replace('_', " "))
}

/// The world clocks: (name, time zone).
pub fn world_clocks() -> Vec<(String, String)> {
    if !helix_settings::has_schema(WORLD_CLOCKS) {
        return Vec::new();
    }
    let locations = gio::Settings::new(WORLD_CLOCKS).value("locations");
    let tab = std::fs::read_to_string("/usr/share/zoneinfo/zone1970.tab").unwrap_or_default();
    locations
        .iter()
        .filter_map(|v| parse_location(&v))
        .filter_map(|(name, lat, lon)| Some((name, nearest_zone(&tab, lat, lon)?)))
        .collect()
}

// ----- Media -------------------------------------------------------------

pub async fn player(conn: &Connection) -> Option<Player> {
    let name = crate::shell_keys::chosen_player(conn).await?;
    let player = zbus::Proxy::new(conn, name.clone(), MPRIS_PATH, MPRIS_PLAYER).await.ok()?;
    let root = zbus::Proxy::new(conn, name.clone(), MPRIS_PATH, "org.mpris.MediaPlayer2").await.ok()?;
    let metadata: HashMap<String, OwnedValue> = player.get_property("Metadata").await.unwrap_or_default();
    let title = metadata.get("xesam:title").and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
    let artist = metadata
        .get("xesam:artist")
        .and_then(|v| Vec::<String>::try_from(v.clone()).ok())
        .map(|a| a.join(", "))
        .unwrap_or_default();
    let identity = root.get_property::<String>("Identity").await.unwrap_or_else(|_| name.trim_start_matches("org.mpris.MediaPlayer2.").to_owned());
    let playing = player.get_property::<String>("PlaybackStatus").await.is_ok_and(|s| s == "Playing");
    Some(Player { identity, title, artist, playing })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources() {
        assert!(is_shown_calendar("[Data Source]\nDisplayName=Personal\nEnabled=true\n\n[Calendar]\nBackendName=local\nSelected=true\n"));
        assert!(is_shown_calendar("[Calendar]\nBackendName=local\n"));
        assert!(!is_shown_calendar("[Calendar]\nSelected=false\n"));
        assert!(!is_shown_calendar("[Data Source]\nEnabled=false\n[Calendar]\n"));
        assert!(!is_shown_calendar("[Data Source]\nEnabled=true\n[Address Book]\n"));
    }

    #[test]
    fn world_clock_zones() {
        use gio::glib::Variant;
        let location = Variant::from_variant(&(2u32, Variant::from_variant(&("Berlin".to_owned(), "EDDB".to_owned(), true, vec![(0.9142616340185987f64, 0.23591034304566436f64)], vec![(0.916f64, 0.2338f64)]).to_variant())).to_variant());
        let (name, lat, lon) = parse_location(&location).unwrap();
        assert_eq!(name, "Berlin");
        assert!((lat - 52.38).abs() < 0.1 && (lon - 13.52).abs() < 0.1);
        let tab = "# comment\nDE,DK,NO,SE,SJ\t+5230+01322\tEurope/Berlin\tmost of Germany\nFR,MC\t+4852+00220\tEurope/Paris\nUS\t+404251-0740023\tAmerica/New_York\tEastern (most areas)\n";
        assert_eq!(nearest_zone(tab, lat, lon).as_deref(), Some("Europe/Berlin"));
        assert_eq!(nearest_zone(tab, 40.71, -74.0).as_deref(), Some("America/New_York"));
        assert_eq!(nearby_city(tab, 40.6, -73.9, 100.0).as_deref(), Some("New York"));
        // Hamburg is some 250 km from Berlin.
        assert_eq!(nearby_city(tab, 53.55, 9.99, 100.0), None);
        assert_eq!(parse_iso6709("+404251-0740023").map(|(a, b)| (a.round(), b.round())), Some((41.0, -74.0)));
    }
}
