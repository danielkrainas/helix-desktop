//! Night Light (helix-desktop-spec `contracts/gsettings/night-light.md`):
//! when it is on, and the gamma ramps that warm the screen, set on every
//! CRTC through DisplayConfig.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use gio::prelude::*;
use zbus::zvariant::OwnedValue;
use zbus::Connection;

pub const SCHEMA: &str = "org.gnome.settings-daemon.plugins.color";
/// The temperature that changes nothing.
pub const NEUTRAL: f64 = 6500.0;
const FADE: Duration = Duration::from_secs(5);
const FADE_STEPS: u32 = 20;
const RECHECK: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub temperature: f64,
    pub automatic: bool,
    pub from: f64,
    pub to: f64,
    /// (latitude, longitude), if usable.
    pub location: Option<(f64, f64)>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: false, temperature: 2700.0, automatic: true, from: 20.0, to: 6.0, location: None }
    }
}

/// Usable coordinates (the schema's default `(91, 181)` means unknown).
pub fn location(lat: f64, lon: f64) -> Option<(f64, f64)> {
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// The current settings (defaults where the schema or a key is missing).
pub fn read() -> Settings {
    let d = Settings::default();
    let Some(schema) = gio::SettingsSchemaSource::default().and_then(|s| s.lookup(SCHEMA, true)) else { return d };
    let s = gio::Settings::new(SCHEMA);
    let has = |k: &str| schema.has_key(k);
    Settings {
        enabled: if has("night-light-enabled") { s.boolean("night-light-enabled") } else { d.enabled },
        temperature: if has("night-light-temperature") { f64::from(s.uint("night-light-temperature")) } else { d.temperature },
        automatic: if has("night-light-schedule-automatic") { s.boolean("night-light-schedule-automatic") } else { d.automatic },
        from: if has("night-light-schedule-from") { s.double("night-light-schedule-from") } else { d.from },
        to: if has("night-light-schedule-to") { s.double("night-light-schedule-to") } else { d.to },
        location: if has("night-light-last-coordinates") {
            s.value("night-light-last-coordinates").get::<(f64, f64)>().and_then(|(lat, lon)| location(lat, lon))
        } else {
            None
        },
    }
}

/// When the sun is up on a day, in local hours.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Daylight {
    Times { sunrise: f64, sunset: f64 },
    AlwaysUp,
    AlwaysDown,
}

/// Sunrise and sunset (NOAA's approximation; the sun's centre 0.833°
/// below the horizon) on day `day_of_year` (1-based) at `lat`, `lon`, for
/// a time zone `utc_offset` hours east of UTC.
pub fn daylight(lat: f64, lon: f64, day_of_year: u32, utc_offset: f64) -> Daylight {
    let g = 2.0 * std::f64::consts::PI / 365.0 * (f64::from(day_of_year) - 1.0);
    let eqtime = 229.18
        * (0.000075 + 0.001868 * g.cos() - 0.032077 * g.sin() - 0.014615 * (2.0 * g).cos() - 0.040849 * (2.0 * g).sin());
    let decl = 0.006918 - 0.399912 * g.cos() + 0.070257 * g.sin() - 0.006758 * (2.0 * g).cos() + 0.000907 * (2.0 * g).sin()
        - 0.002697 * (3.0 * g).cos()
        + 0.00148 * (3.0 * g).sin();
    let lat = lat.to_radians();
    let cos_ha = 90.833f64.to_radians().cos() / (lat.cos() * decl.cos()) - lat.tan() * decl.tan();
    if cos_ha > 1.0 {
        return Daylight::AlwaysDown;
    }
    if cos_ha < -1.0 {
        return Daylight::AlwaysUp;
    }
    let ha = cos_ha.acos().to_degrees();
    let local = |utc_minutes: f64| (utc_minutes / 60.0 + utc_offset).rem_euclid(24.0);
    Daylight::Times {
        sunrise: local(720.0 - 4.0 * (lon + ha) - eqtime),
        sunset: local(720.0 - 4.0 * (lon - ha) - eqtime),
    }
}

/// Whether `hour` is in the window from `from` to `to` (local hours,
/// possibly over midnight). Equal times mean never.
pub fn in_window(from: f64, to: f64, hour: f64) -> bool {
    if from < to {
        (from..to).contains(&hour)
    } else if from > to {
        hour >= from || hour < to
    } else {
        false
    }
}

/// Whether Night Light is on now.
pub fn is_on(settings: &Settings, day_of_year: u32, hour: f64, utc_offset: f64) -> bool {
    if !settings.enabled {
        return false;
    }
    match (settings.automatic, settings.location) {
        (true, Some((lat, lon))) => match daylight(lat, lon, day_of_year, utc_offset) {
            Daylight::Times { sunrise, sunset } => in_window(sunset, sunrise, hour),
            Daylight::AlwaysUp => false,
            Daylight::AlwaysDown => true,
        },
        _ => in_window(settings.from, settings.to, hour),
    }
}

/// Tanner Helland's fit of blackbody colors, 0 to 255 per channel.
fn blackbody(kelvin: f64) -> (f64, f64, f64) {
    let t = kelvin / 100.0;
    let red = if t <= 66.0 { 255.0 } else { 329.698727446 * (t - 60.0).powf(-0.1332047592) };
    let green = if t <= 66.0 { 99.4708025861 * t.ln() - 161.1195681661 } else { 288.1221695283 * (t - 60.0).powf(-0.0755148492) };
    let blue = if t >= 66.0 {
        255.0
    } else if t <= 19.0 {
        0.0
    } else {
        138.5177312231 * (t - 10.0).ln() - 305.0447927307
    };
    let c = |v: f64| v.clamp(0.0, 255.0);
    (c(red), c(green), c(blue))
}

/// Channel factors for `kelvin`, 1 at 6500 K.
pub fn white_point(kelvin: f64) -> (f64, f64, f64) {
    let (r, g, b) = blackbody(kelvin.clamp(1000.0, 10000.0));
    let (nr, ng, nb) = blackbody(NEUTRAL);
    ((r / nr).min(1.0), (g / ng).min(1.0), (b / nb).min(1.0))
}

/// Linear ramps of `size` entries scaled by the white point of `kelvin`.
pub fn ramps(size: usize, kelvin: f64) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    let (r, g, b) = white_point(kelvin);
    let ramp = |factor: f64| -> Vec<u16> {
        (0..size)
            .map(|i| {
                let x = if size > 1 { i as f64 / (size - 1) as f64 } else { 1.0 };
                (x * factor * 65535.0).round() as u16
            })
            .collect()
    };
    (ramp(r), ramp(g), ramp(b))
}

const DC: &str = "org.gnome.Mutter.DisplayConfig";
const DC_PATH: &str = "/org/gnome/Mutter/DisplayConfig";

type Resources = (
    u32,
    Vec<(u32, i64, i32, i32, i32, i32, i32, u32, Vec<u32>, HashMap<String, OwnedValue>)>,
    Vec<(u32, i64, i32, Vec<u32>, String, Vec<u32>, Vec<u32>, HashMap<String, OwnedValue>)>,
    Vec<(u32, i64, u32, u32, f64, u32)>,
    i32,
    i32,
);

/// Warm every CRTC to `kelvin`.
pub async fn set_temperature(conn: &Connection, kelvin: f64) -> zbus::Result<()> {
    let reply = conn.call_method(Some(DC), DC_PATH, Some(DC), "GetResources", &()).await?;
    let (serial, crtcs, ..): Resources = reply.body().deserialize()?;
    for crtc in crtcs.iter().map(|c| c.0) {
        let reply = conn.call_method(Some(DC), DC_PATH, Some(DC), "GetCrtcGamma", &(serial, crtc)).await?;
        let (red, _, _): (Vec<u16>, Vec<u16>, Vec<u16>) = reply.body().deserialize()?;
        let (r, g, b) = ramps(red.len(), kelvin);
        conn.call_method(Some(DC), DC_PATH, Some(DC), "SetCrtcGamma", &(serial, crtc, r, g, b)).await?;
    }
    Ok(())
}

pub const BUS_NAME: &str = "org.gnome.SettingsDaemon.Color";
pub const OBJECT_PATH: &str = "/org/gnome/SettingsDaemon/Color";
/// Longest preview, in seconds.
pub const MAX_PREVIEW: u32 = 20;

/// Night Light as published on `org.gnome.SettingsDaemon.Color`.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub active: bool,
    pub temperature: u32,
    pub disabled_until_tomorrow: bool,
    pub sunrise: f64,
    pub sunset: f64,
}

impl Default for Status {
    fn default() -> Self {
        Self { active: false, temperature: NEUTRAL as u32, disabled_until_tomorrow: false, sunrise: -1.0, sunset: -1.0 }
    }
}

#[derive(Debug, Default)]
struct State {
    status: Status,
    preview_until: Option<tokio::time::Instant>,
    /// The schedule said "on" last time (to end "until tomorrow").
    was_scheduled: bool,
}

/// Shared between the D-Bus object and the runner.
#[derive(Debug, Clone, Default)]
pub struct Handle {
    state: Arc<Mutex<State>>,
    wake: Arc<tokio::sync::Notify>,
}

impl Handle {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn status(&self) -> Status {
        self.state().status.clone()
    }
}

/// What Night Light does now, given the schedule, "disabled until
/// tomorrow" and a preview. Returns (target kelvin, active, disabled flag
/// after this check).
pub fn decide(scheduled: bool, was_scheduled: bool, disabled: bool, previewing: bool, temperature: f64) -> (f64, bool, bool) {
    // The period ended: "until tomorrow" is over.
    let disabled = disabled && !(was_scheduled && !scheduled);
    let on = previewing || (scheduled && !disabled);
    (if on { temperature } else { NEUTRAL }, on, disabled)
}

/// The served `org.gnome.SettingsDaemon.Color` object.
pub struct ColorService {
    pub handle: Handle,
}

#[zbus::interface(name = "org.gnome.SettingsDaemon.Color")]
impl ColorService {
    async fn night_light_preview(&self, duration: u32) {
        let seconds = u64::from(duration.clamp(1, MAX_PREVIEW));
        self.handle.state().preview_until = Some(tokio::time::Instant::now() + Duration::from_secs(seconds));
        tracing::info!(seconds, "night light preview");
        self.handle.wake.notify_one();
    }

    #[zbus(property)]
    async fn night_light_active(&self) -> bool {
        self.handle.status().active
    }

    #[zbus(property)]
    async fn temperature(&self) -> u32 {
        self.handle.status().temperature
    }

    #[zbus(property)]
    async fn set_temperature(&self, _value: u32) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::PropertyReadOnly("set night-light-temperature instead".into()))
    }

    #[zbus(property)]
    async fn disabled_until_tomorrow(&self) -> bool {
        self.handle.status().disabled_until_tomorrow
    }

    #[zbus(property)]
    async fn set_disabled_until_tomorrow(&self, value: bool) {
        self.handle.state().status.disabled_until_tomorrow = value;
        self.handle.wake.notify_one();
    }

    #[zbus(property)]
    async fn sunrise(&self) -> f64 {
        self.handle.status().sunrise
    }

    #[zbus(property)]
    async fn sunset(&self) -> f64 {
        self.handle.status().sunset
    }
}

/// Work out the target and publish what changed.
async fn check(conn: &Connection, handle: &Handle) -> f64 {
    let settings = read();
    let now = gio::glib::DateTime::now_local().ok();
    let (day, hour, offset) = match &now {
        Some(now) => (
            now.day_of_year() as u32,
            f64::from(now.hour()) + f64::from(now.minute()) / 60.0,
            now.utc_offset().as_seconds() as f64 / 3600.0,
        ),
        None => (1, 12.0, 0.0),
    };
    let scheduled = now.is_some() && is_on(&Settings { enabled: settings.enabled, ..settings }, day, hour, offset);
    let (sunrise, sunset) = match settings.location.map(|(lat, lon)| daylight(lat, lon, day, offset)) {
        Some(Daylight::Times { sunrise, sunset }) => (sunrise, sunset),
        _ => (-1.0, -1.0),
    };
    let temperature = settings.temperature.clamp(1000.0, 10000.0);
    let (target, old, new) = {
        let mut state = handle.state();
        let previewing = state.preview_until.is_some_and(|t| t > tokio::time::Instant::now());
        if !previewing {
            state.preview_until = None;
        }
        let (target, active, disabled) = decide(scheduled, state.was_scheduled, state.status.disabled_until_tomorrow, previewing, temperature);
        state.was_scheduled = scheduled;
        let old = state.status.clone();
        state.status = Status { active, temperature: target as u32, disabled_until_tomorrow: disabled, sunrise, sunset };
        (target, old, state.status.clone())
    };
    if old != new {
        if let Ok(iface) = conn.object_server().interface::<_, ColorService>(OBJECT_PATH).await {
            let (service, emitter) = (iface.get().await, iface.signal_emitter().clone());
            if old.active != new.active {
                let _ = service.night_light_active_changed(&emitter).await;
            }
            if old.temperature != new.temperature {
                let _ = service.temperature_changed(&emitter).await;
            }
            if old.disabled_until_tomorrow != new.disabled_until_tomorrow {
                let _ = service.disabled_until_tomorrow_changed(&emitter).await;
            }
            if old.sunrise != new.sunrise {
                let _ = service.sunrise_changed(&emitter).await;
            }
            if old.sunset != new.sunset {
                let _ = service.sunset_changed(&emitter).await;
            }
        }
    }
    target
}

/// Run Night Light: follow the settings, the clock, monitor changes and
/// requests on `org.gnome.SettingsDaemon.Color`.
pub async fn run(conn: Connection, handle: Handle) {
    let watched = helix_settings::GSettings::new(&[SCHEMA]);
    let (mut changes, _watch) = watched.watch();
    let monitors = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(DC)
        .and_then(|b| b.member("MonitorsChanged"))
        .map(|b| b.build());
    let mut monitors_changed = match monitors {
        Ok(rule) => zbus::MessageStream::for_match_rule(rule, &conn, None).await.ok(),
        Err(_) => None,
    };
    // `None`: never touched (leave the gamma alone while off).
    let mut current: Option<f64> = None;
    loop {
        let target = check(&conn, &handle).await;
        let touched = current.is_some_and(|c| c != NEUTRAL);
        if current != Some(target) && (target != NEUTRAL || touched) {
            let from = current.unwrap_or(NEUTRAL);
            tracing::info!(from, to = target, "night light");
            for step in 1..=FADE_STEPS {
                let kelvin = from + (target - from) * f64::from(step) / f64::from(FADE_STEPS);
                if let Err(err) = set_temperature(&conn, kelvin).await {
                    tracing::warn!(%err, "cannot set the gamma; night light unavailable");
                    break;
                }
                tokio::time::sleep(FADE / FADE_STEPS).await;
            }
            current = Some(target);
        }
        let monitor_change = async {
            match monitors_changed.as_mut() {
                Some(stream) => {
                    stream.next().await;
                }
                None => std::future::pending().await,
            }
        };
        // Wake when a preview ends, too.
        let preview_end = handle.state().preview_until;
        let next = preview_end.map_or(RECHECK, |t| t.saturating_duration_since(tokio::time::Instant::now()).min(RECHECK));
        tokio::select! {
            _ = tokio::time::sleep(next) => {}
            _ = handle.wake.notified() => {}
            Some(_) = changes.recv() => {}
            _ = monitor_change => {
                // New CRTCs start neutral: set the gamma again.
                if let Some(kelvin) = current.filter(|c| *c != NEUTRAL) {
                    let _ = set_temperature(&conn, kelvin).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_schedule() {
        assert!(in_window(20.0, 6.0, 23.5));
        assert!(in_window(20.0, 6.0, 2.0));
        assert!(!in_window(20.0, 6.0, 12.0));
        assert!(in_window(8.0, 9.5, 9.0));
        assert!(!in_window(8.0, 9.5, 9.5));
        assert!(!in_window(7.0, 7.0, 7.0), "equal times: never");
        let s = Settings { enabled: true, automatic: false, ..Settings::default() };
        assert!(is_on(&s, 100, 21.0, 0.0));
        assert!(!is_on(&Settings { enabled: false, ..s }, 100, 21.0, 0.0));
        // Automatic without a location uses the manual times.
        assert!(is_on(&Settings { automatic: true, ..s }, 100, 21.0, 0.0));
    }

    #[test]
    fn sunrise_and_sunset() {
        // Equator, prime meridian, March equinox: about 06:00 and 18:10 UTC.
        let Daylight::Times { sunrise, sunset } = daylight(0.0, 0.0, 80, 0.0) else { panic!() };
        assert!((5.8..6.3).contains(&sunrise), "{sunrise}");
        assert!((17.9..18.4).contains(&sunset), "{sunset}");
        // Berlin (UTC+2) on 21 June: about 04:45 and 21:30 local.
        let Daylight::Times { sunrise, sunset } = daylight(52.52, 13.40, 172, 2.0) else { panic!() };
        assert!((4.5..5.1).contains(&sunrise), "{sunrise}");
        assert!((21.2..21.8).contains(&sunset), "{sunset}");
        assert_eq!(daylight(78.2, 15.6, 172, 2.0), Daylight::AlwaysUp, "Svalbard midsummer");
        assert_eq!(daylight(78.2, 15.6, 355, 1.0), Daylight::AlwaysDown, "Svalbard midwinter");
        let s = Settings { enabled: true, automatic: true, location: Some((52.52, 13.40)), ..Settings::default() };
        assert!(is_on(&s, 172, 23.0, 2.0));
        assert!(!is_on(&s, 172, 12.0, 2.0));
        assert!(location(91.0, 181.0).is_none(), "the schema's 'unknown'");
    }

    #[test]
    fn until_tomorrow_and_preview() {
        let t = 2700.0;
        assert_eq!(decide(true, true, false, false, t), (t, true, false), "scheduled");
        assert_eq!(decide(true, true, true, false, t), (NEUTRAL, false, true), "disabled until tomorrow");
        assert_eq!(decide(false, true, true, false, t), (NEUTRAL, false, false), "period over: cleared");
        assert_eq!(decide(false, false, true, false, t), (NEUTRAL, false, true), "set while off: kept for the next period");
        assert_eq!(decide(true, false, true, false, t), (NEUTRAL, false, true), "...which it skips");
        assert_eq!(decide(false, false, false, true, t), (t, true, false), "preview");
    }

    #[test]
    fn warm_white_points() {
        let (r, g, b) = white_point(6500.0);
        assert_eq!((r, g, b), (1.0, 1.0, 1.0));
        let (r, g, b) = white_point(2700.0);
        assert_eq!(r, 1.0);
        assert!(g < 0.8 && b < g, "{g} {b}");
        let (r, g, b) = ramps(3, 6500.0);
        assert_eq!(r, [0, 32768, 65535]);
        assert_eq!((g.len(), b.len()), (3, 3));
        assert_eq!(*ramps(2, 2700.0).2.last().unwrap(), (white_point(2700.0).2 * 65535.0).round() as u16);
    }
}
