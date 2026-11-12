//! The weather in the message tray (helix-desktop-spec
//! `contracts/gsettings/calendar.md`, "Weather"): MET Norway's location
//! forecast for the first configured location.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use gio::prelude::*;

const SCHEMA: &str = "org.gnome.shell.weather";
const GWEATHER: &str = "org.gnome.GWeather4";
const DEFAULT_URL: &str = "https://api.met.no/weatherapi/locationforecast/2.0/compact";
const REFRESH: Duration = Duration::from_secs(30 * 60);
pub const HOURS: usize = 4;

/// A forecast: now and the next hours (Unix time, °C, symbol code).
#[derive(Debug, Clone, PartialEq)]
pub struct Forecast {
    pub location: String,
    pub temperature: f64,
    pub symbol: String,
    pub hours: Vec<(i64, f64, String)>,
}

/// An hour of the forecast: Unix time, °C, symbol code.
pub type Hour = (i64, f64, String);

/// From MET Norway's compact JSON: the entry at or just before `now`, and
/// the next hours.
pub fn parse_forecast(json: &str, now: i64) -> Option<(f64, String, Vec<Hour>)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let series = v.get("properties")?.get("timeseries")?.as_array()?;
    let entries: Vec<(i64, f64, String)> = series
        .iter()
        .filter_map(|e| {
            let time = gio::glib::DateTime::from_iso8601(e.get("time")?.as_str()?, None).ok()?.to_unix();
            let data = e.get("data")?;
            let temp = data.get("instant")?.get("details")?.get("air_temperature")?.as_f64()?;
            let symbol = ["next_1_hours", "next_6_hours"]
                .iter()
                .find_map(|k| data.get(*k)?.get("summary")?.get("symbol_code")?.as_str())
                .unwrap_or("")
                .to_owned();
            Some((time, temp, symbol))
        })
        .collect();
    let current = entries.iter().rposition(|e| e.0 <= now).unwrap_or(0);
    let (_, temp, symbol) = entries.get(current)?.clone();
    let hours = entries.iter().skip(current + 1).take(HOURS).cloned().collect();
    Some((temp, symbol, hours))
}

/// A symbol code's words and icon.
pub fn condition(symbol: &str) -> (&'static str, &'static str) {
    let night = symbol.ends_with("_night") || symbol.ends_with("_polartwilight");
    let base = symbol.split('_').next().unwrap_or("");
    match base {
        "clearsky" if night => ("Clear", "weather-clear-night-symbolic"),
        "clearsky" => ("Clear", "weather-clear-symbolic"),
        "fair" | "partlycloudy" => {
            let words = if base == "fair" { "Mostly Clear" } else { "Partly Cloudy" };
            (words, if night { "weather-few-clouds-night-symbolic" } else { "weather-few-clouds-symbolic" })
        }
        "cloudy" => ("Cloudy", "weather-overcast-symbolic"),
        "fog" => ("Fog", "weather-fog-symbolic"),
        b if b.contains("thunder") => ("Thunderstorm", "weather-storm-symbolic"),
        b if b.contains("snow") => ("Snow", "weather-snow-symbolic"),
        b if b.contains("sleet") => ("Sleet", "weather-snow-symbolic"),
        "lightrain" | "lightrainshowers" => ("Light Rain", "weather-showers-scattered-symbolic"),
        b if b.contains("rain") || b.contains("showers") => ("Rain", "weather-showers-symbolic"),
        _ => ("Unknown", "weather-severe-alert-symbolic"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Celsius,
    Fahrenheit,
    Kelvin,
}

/// Whole degrees in `unit`.
pub fn temperature_text(celsius: f64, unit: Unit) -> String {
    match unit {
        Unit::Celsius => format!("{}°C", celsius.round() as i64),
        Unit::Fahrenheit => format!("{}°F", (celsius * 9.0 / 5.0 + 32.0).round() as i64),
        Unit::Kelvin => format!("{} K", (celsius + 273.15).round() as i64),
    }
}

/// `temperature-unit`, `default` by the locale's measurement system.
pub fn unit_for(setting: &str, measurement: Option<u32>) -> Unit {
    match setting {
        "centigrade" => Unit::Celsius,
        "fahrenheit" => Unit::Fahrenheit,
        "kelvin" => Unit::Kelvin,
        _ if measurement == Some(2) => Unit::Fahrenheit,
        _ => Unit::Celsius,
    }
}

pub fn unit() -> Unit {
    let setting = if helix_settings::has_schema(GWEATHER) { gio::Settings::new(GWEATHER).string("temperature-unit").to_string() } else { "default".into() };
    let measurement = std::process::Command::new("locale").arg("measurement").output().ok().and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok());
    unit_for(&setting, measurement)
}

/// The first configured location: name, latitude, longitude (degrees).
fn location() -> Option<(String, f64, f64)> {
    if !helix_settings::has_schema(SCHEMA) {
        return None;
    }
    let locations = gio::Settings::new(SCHEMA).value("locations");
    locations.iter().find_map(|v| crate::calendar::parse_location(&v))
}

fn fetch(lat: f64, lon: f64) -> Result<String, String> {
    let base = std::env::var("HELIX_WEATHER_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned());
    let mut agent = format!("helix-desktop/{}", env!("CARGO_PKG_VERSION"));
    if !env!("CARGO_PKG_REPOSITORY").is_empty() {
        agent.push_str(&format!(" {}", env!("CARGO_PKG_REPOSITORY")));
    }
    let url = format!("{base}?lat={lat:.4}&lon={lon:.4}");
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .user_agent(&agent)
        .build()
        .get(&url)
        .call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())
}

type Cache = HashMap<(i64, i64), (Instant, Option<Forecast>)>;
static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

/// The forecast for the first location, when one is configured: `None`
/// without a location, `Some(None)` when it could not be had.
pub fn current() -> Option<Option<Forecast>> {
    let (name, lat, lon) = location()?;
    let key = ((lat * 1e4).round() as i64, (lon * 1e4).round() as i64);
    {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, forecast)) = cache.as_ref().and_then(|c| c.get(&key)) {
            if at.elapsed() < REFRESH {
                return Some(forecast.clone());
            }
        }
    }
    let now = gio::glib::DateTime::now_utc().map(|d| d.to_unix()).unwrap_or(0);
    let forecast = match fetch(lat, lon) {
        Ok(json) => parse_forecast(&json, now).map(|(temperature, symbol, hours)| Forecast { location: name, temperature, symbol, hours }),
        Err(err) => {
            tracing::info!(%err, "no weather forecast");
            None
        }
    };
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(key, (Instant::now(), forecast.clone()));
    Some(forecast)
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{"properties":{"timeseries":[
      {"time":"2026-11-12T10:00:00Z","data":{"instant":{"details":{"air_temperature":4.2}},"next_1_hours":{"summary":{"symbol_code":"cloudy"}}}},
      {"time":"2026-11-12T11:00:00Z","data":{"instant":{"details":{"air_temperature":5.6}},"next_1_hours":{"summary":{"symbol_code":"lightrain"}}}},
      {"time":"2026-11-12T12:00:00Z","data":{"instant":{"details":{"air_temperature":6.1}},"next_6_hours":{"summary":{"symbol_code":"rain"}}}},
      {"time":"2026-11-12T13:00:00Z","data":{"instant":{"details":{"air_temperature":6.4}}}}
    ]}}"#;

    fn utc(h: i32, m: i32) -> i64 {
        gio::glib::DateTime::new(&gio::glib::TimeZone::utc(), 2026, 11, 12, h, m, 0.0).unwrap().to_unix()
    }

    #[test]
    fn forecasts() {
        let (temp, symbol, hours) = parse_forecast(JSON, utc(11, 20)).unwrap();
        assert_eq!((temp, symbol.as_str()), (5.6, "lightrain"));
        assert_eq!(hours.len(), 2);
        assert_eq!(hours[0], (utc(12, 0), 6.1, "rain".to_owned()));
        assert_eq!(parse_forecast(JSON, utc(9, 0)).unwrap().0, 4.2, "before the first entry: the first");
        assert!(parse_forecast("{}", 0).is_none());
    }

    #[test]
    fn words_and_units() {
        assert_eq!(condition("clearsky_night"), ("Clear", "weather-clear-night-symbolic"));
        assert_eq!(condition("partlycloudy_day").0, "Partly Cloudy");
        assert_eq!(condition("heavyrainandthunder").0, "Thunderstorm");
        assert_eq!(condition("lightrainshowers_day").0, "Light Rain");
        assert_eq!(condition("heavysnow").0, "Snow");
        assert_eq!(temperature_text(21.6, Unit::Celsius), "22°C");
        assert_eq!(temperature_text(0.0, Unit::Fahrenheit), "32°F");
        assert_eq!(unit_for("default", Some(2)), Unit::Fahrenheit);
        assert_eq!(unit_for("default", Some(1)), Unit::Celsius);
        assert_eq!(unit_for("kelvin", Some(2)), Unit::Kelvin);
    }
}
