//! What the message tray's calendar column shows (helixdesktop/spec
//! `contracts/gsettings/calendar.md`): events from Evolution Data Server,
//! world clocks, and the media player.

use std::collections::HashMap;
use std::time::{Duration, Instant};

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
    /// The track's art (`mpris:artUrl`), and the player's desktop file
    /// id (`DesktopEntry`): may be empty.
    pub art_url: String,
    pub desktop_entry: String,
    /// Where playback was (`Position`, µs) when read, and the track's
    /// length (`mpris:length`, µs): `None` when the player does not say.
    pub position: Option<i64>,
    pub length: Option<i64>,
    /// The playback rate (`Rate`): `None` for the usual 1.0.
    pub rate: Option<f64>,
    /// When `position` was read: while playing, the position moves on
    /// from there (`None`: it stays put, as in pictures).
    pub read_at: Option<Instant>,
}

impl Player {
    /// How far into the track playback is at `now` (0.0–1.0): `None`
    /// when the player reports no position or no length (no bar then).
    pub fn progress(&self, now: Instant) -> Option<f64> {
        let (position, length) = (self.position?, self.length.filter(|l| *l > 0)?);
        let moved = match self.read_at {
            Some(at) if self.playing => now.saturating_duration_since(at).as_secs_f64() * 1e6 * self.rate.unwrap_or(1.0),
            _ => 0.0,
        };
        Some(((position as f64 + moved) / length as f64).clamp(0.0, 1.0))
    }
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
    let locations = helix_settings::Settings::new(WORLD_CLOCKS).value("locations");
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
    let mut art_url = metadata.get("mpris:artUrl").and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
    // Web art is downloaded only for players that may use the network.
    if !art_url.starts_with("file://") && !art_url.is_empty() && !crate::caller::may_use_network(conn, &name).await {
        art_url.clear();
    }
    let desktop_entry = root.get_property::<String>("DesktopEntry").await.unwrap_or_default();
    let length = metadata.get("mpris:length").and_then(micros);
    let position = player.get_property::<i64>("Position").await.ok().filter(|p| *p >= 0);
    let rate = player.get_property::<f64>("Rate").await.ok().filter(|r| *r > 0.0 && (*r - 1.0).abs() > f64::EPSILON);
    Some(Player { identity, title, artist, playing, art_url, desktop_entry, position, length, rate, read_at: Some(Instant::now()) })
}

/// A time in µs as players send it: `x` per the spec, but some send `t`,
/// `i` or `u`.
fn micros(v: &OwnedValue) -> Option<i64> {
    use zbus::zvariant::Value;
    match &**v {
        Value::I64(x) => Some(*x),
        Value::U64(x) => i64::try_from(*x).ok(),
        Value::I32(x) => Some(i64::from(*x)),
        Value::U32(x) => Some(i64::from(*x)),
        _ => None,
    }
    .filter(|l| *l > 0)
}

/// Follow the media players for the rest of the session: when one seeks
/// (`Seeked`) or its state changes (`PropertiesChanged` of the player
/// interface: a new track, play/pause), the chosen player is read again
/// and handed to the UI, which shows it in the media cards it has open.
pub async fn follow_players(conn: Connection, ui: tokio::sync::mpsc::UnboundedSender<crate::UiEvent>) -> zbus::Result<()> {
    use futures_util::StreamExt;
    let seeked = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).path(MPRIS_PATH)?.interface(MPRIS_PLAYER)?.member("Seeked")?.build();
    let changed = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .path(MPRIS_PATH)?
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .arg(0, MPRIS_PLAYER)?
        .build();
    let mut seeked = zbus::MessageStream::for_match_rule(seeked, &conn, Some(16)).await?;
    let mut changed = zbus::MessageStream::for_match_rule(changed, &conn, Some(16)).await?;
    loop {
        tokio::select! {
            m = seeked.next() => if m.is_none() { break },
            m = changed.next() => if m.is_none() { break },
        }
        // Players send several at once (a new track: metadata, then
        // status): read once they settle.
        tokio::time::sleep(Duration::from_millis(150)).await;
        while futures_util::FutureExt::now_or_never(seeked.next()).flatten().is_some() {}
        while futures_util::FutureExt::now_or_never(changed.next()).flatten().is_some() {}
        if ui.send(crate::UiEvent::MediaPlayer(player(&conn).await)).is_err() {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gio::prelude::*;

    #[test]
    fn sources() {
        assert!(is_shown_calendar("[Data Source]\nDisplayName=Personal\nEnabled=true\n\n[Calendar]\nBackendName=local\nSelected=true\n"));
        assert!(is_shown_calendar("[Calendar]\nBackendName=local\n"));
        assert!(!is_shown_calendar("[Calendar]\nSelected=false\n"));
        assert!(!is_shown_calendar("[Data Source]\nEnabled=false\n[Calendar]\n"));
        assert!(!is_shown_calendar("[Data Source]\nEnabled=true\n[Address Book]\n"));
    }

    #[test]
    fn progress_moves_on_while_playing() {
        let now = Instant::now();
        let at = now - Duration::from_secs(10);
        let player = Player { position: Some(20_000_000), length: Some(100_000_000), read_at: Some(at), playing: true, ..Default::default() };
        assert!((player.progress(now).unwrap() - 0.3).abs() < 1e-6);
        // Paused (or never read): it stays put.
        assert!((Player { playing: false, ..player.clone() }.progress(now).unwrap() - 0.2).abs() < 1e-6);
        assert!((Player { read_at: None, ..player.clone() }.progress(now).unwrap() - 0.2).abs() < 1e-6);
        // At twice the speed; never past the end.
        assert!((Player { rate: Some(2.0), ..player.clone() }.progress(now).unwrap() - 0.4).abs() < 1e-6);
        assert_eq!(Player { position: Some(99_000_000), ..player.clone() }.progress(now), Some(1.0));
        // No bar without both.
        assert_eq!(Player { length: None, ..player.clone() }.progress(now), None);
        assert_eq!(Player { length: Some(0), ..player.clone() }.progress(now), None);
        assert_eq!(Player { position: None, ..player }.progress(now), None);
    }

    #[test]
    fn lengths_of_any_integer_type() {
        use zbus::zvariant::Value;
        let owned = |v: Value| OwnedValue::try_from(v).unwrap();
        assert_eq!(micros(&owned(Value::I64(5))), Some(5));
        assert_eq!(micros(&owned(Value::U64(5))), Some(5));
        assert_eq!(micros(&owned(Value::I32(5))), Some(5));
        assert_eq!(micros(&owned(Value::U32(5))), Some(5));
        assert_eq!(micros(&owned(Value::I64(0))), None);
        assert_eq!(micros(&owned(Value::from("5"))), None);
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
