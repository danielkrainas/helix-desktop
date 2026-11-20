//! `GetPower` (helixdesktop/spec `contracts/agent-access/README.md`,
//! "Records", Power, and "Media, power and stars";
//! helixdesktop/desktop#156): the battery as UPower tells it and the power
//! mode as power-profiles-daemon tells it, read from the system bus when
//! the call comes, as the panel and quick settings read them. Nothing is
//! started for it; what the computer does not have is left out.

use std::collections::HashMap;
use std::time::Duration;

use zbus::zvariant::OwnedValue;
use zbus::Connection;

use super::value;
use crate::status::{profiles_service, prop, PROFILES_PATH};

type Record = HashMap<String, OwnedValue>;

const UPOWER: &str = "org.freedesktop.UPower";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";
const DISPLAY: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const DEVICE: &str = "org.freedesktop.UPower.Device";
/// UPower's device type of a battery.
const BATTERY: u32 = 2;
/// How long each service has to answer.
const ANSWER: Duration = Duration::from_secs(3);

/// UPower's `State` as the Power record names it (`None`: unknown).
pub fn battery_state(state: u32) -> Option<&'static str> {
    Some(match state {
        1 => "charging",
        2 => "discharging",
        3 => "empty",
        4 => "fully-charged",
        5 => "pending-charge",
        6 => "pending-discharge",
        _ => return None,
    })
}

/// UPower's `WarningLevel` as the Power record names it (`None`: unknown,
/// and `discharging`, which only UPSes have).
pub fn warning_level(level: u32) -> Option<&'static str> {
    Some(match level {
        1 => "none",
        3 => "low",
        4 => "critical",
        5 => "action",
        _ => return None,
    })
}

/// UPower's part of the Power record.
async fn upower(system: &Connection, out: &mut Record) {
    if let Some(on) = prop::<bool>(system, UPOWER, UPOWER_PATH, UPOWER, "OnBattery").await {
        out.insert("on-battery".to_owned(), value(on));
    }
    let present = prop::<bool>(system, UPOWER, DISPLAY, DEVICE, "IsPresent").await == Some(true);
    if !present || prop::<u32>(system, UPOWER, DISPLAY, DEVICE, "Type").await != Some(BATTERY) {
        return;
    }
    if let Some(percent) = prop::<f64>(system, UPOWER, DISPLAY, DEVICE, "Percentage").await {
        out.insert("percentage".to_owned(), value(percent.clamp(0.0, 100.0)));
    }
    if let Some(state) = prop::<u32>(system, UPOWER, DISPLAY, DEVICE, "State").await.and_then(battery_state) {
        out.insert("battery-state".to_owned(), value(state));
    }
    for (key, name) in [("time-to-empty", "TimeToEmpty"), ("time-to-full", "TimeToFull")] {
        if let Some(secs) = prop::<i64>(system, UPOWER, DISPLAY, DEVICE, name).await.filter(|s| *s > 0) {
            out.insert(key.to_owned(), value(secs));
        }
    }
    if let Some(level) = prop::<u32>(system, UPOWER, DISPLAY, DEVICE, "WarningLevel").await.and_then(warning_level) {
        out.insert("warning-level".to_owned(), value(level));
    }
}

/// power-profiles-daemon's part of the Power record.
async fn profiles(system: &Connection, out: &mut Record) {
    let Some((name, iface)) = profiles_service(system).await else { return };
    if let Some(active) = prop::<String>(system, name, PROFILES_PATH, iface, "ActiveProfile").await.filter(|p| !p.is_empty()) {
        out.insert("power-profile".to_owned(), value(active));
    }
    if let Some(list) = prop::<Vec<HashMap<String, OwnedValue>>>(system, name, PROFILES_PATH, iface, "Profiles").await {
        let names: Vec<String> = list.iter().filter_map(|p| p.get("Profile").and_then(|v| String::try_from(v.clone()).ok())).filter(|p| !p.is_empty()).collect();
        out.insert("power-profiles".to_owned(), value(names));
    }
}

/// The Power record from `system` (no system bus: `{}`).
pub async fn read(system: Option<&Connection>) -> Record {
    let mut out = Record::new();
    let Some(system) = system else { return out };
    let mut battery = Record::new();
    if tokio::time::timeout(ANSWER, upower(system, &mut battery)).await.is_ok() {
        out.extend(battery);
    }
    let mut modes = Record::new();
    if tokio::time::timeout(ANSWER, profiles(system, &mut modes)).await.is_ok() {
        out.extend(modes);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(battery_state(1), Some("charging"));
        assert_eq!(battery_state(4), Some("fully-charged"));
        assert_eq!(battery_state(0), None);
        assert_eq!(warning_level(1), Some("none"));
        assert_eq!(warning_level(2), None, "UPSes only");
        assert_eq!(warning_level(5), Some("action"));
    }
}
