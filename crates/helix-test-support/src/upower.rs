//! Fake UPower and power-profiles-daemon for tests, served on a private
//! bus that stands in for the system bus (never the real daemons).

use std::collections::HashMap;

use zbus::interface;
use zbus::zvariant::{OwnedValue, Value};

use crate::TestBus;

/// What the fake UPower says.
#[derive(Debug, Clone)]
pub struct Battery {
    pub on_battery: bool,
    /// UPower's device type (2: a battery, 1: line power).
    pub kind: u32,
    pub percentage: f64,
    /// UPower's `State` (2: discharging).
    pub state: u32,
    pub time_to_empty: i64,
    pub time_to_full: i64,
    /// UPower's `WarningLevel` (1: none).
    pub warning_level: u32,
}

impl Default for Battery {
    fn default() -> Self {
        Battery { on_battery: true, kind: 2, percentage: 54.0, state: 2, time_to_empty: 7800, time_to_full: 0, warning_level: 1 }
    }
}

struct Manager(bool);

#[interface(name = "org.freedesktop.UPower")]
impl Manager {
    #[zbus(property)]
    fn on_battery(&self) -> bool {
        self.0
    }
}

struct Device(Battery);

#[interface(name = "org.freedesktop.UPower.Device")]
impl Device {
    #[zbus(property)]
    fn is_present(&self) -> bool {
        true
    }
    #[zbus(property, name = "Type")]
    fn kind(&self) -> u32 {
        self.0.kind
    }
    #[zbus(property)]
    fn percentage(&self) -> f64 {
        self.0.percentage
    }
    #[zbus(property)]
    fn state(&self) -> u32 {
        self.0.state
    }
    #[zbus(property)]
    fn time_to_empty(&self) -> i64 {
        self.0.time_to_empty
    }
    #[zbus(property)]
    fn time_to_full(&self) -> i64 {
        self.0.time_to_full
    }
    #[zbus(property)]
    fn warning_level(&self) -> u32 {
        self.0.warning_level
    }
    #[zbus(property)]
    fn icon_name(&self) -> String {
        "battery-level-50-symbolic".into()
    }
}

struct Profiles {
    active: String,
    all: Vec<String>,
}

#[interface(name = "org.freedesktop.UPower.PowerProfiles")]
impl Profiles {
    #[zbus(property)]
    fn active_profile(&self) -> String {
        self.active.clone()
    }
    #[zbus(property)]
    fn profiles(&self) -> Vec<HashMap<String, OwnedValue>> {
        self.all
            .iter()
            .map(|p| HashMap::from([("Profile".to_owned(), OwnedValue::try_from(Value::from(p.as_str())).unwrap()), ("Driver".to_owned(), OwnedValue::try_from(Value::from("placeholder")).unwrap())]))
            .collect()
    }
}

/// Serve UPower with `battery` (and its display device), and
/// power-profiles-daemon with `profiles` (the active one, and all in
/// order), on `bus`; they go when the connection is dropped.
pub async fn serve(bus: &TestBus, battery: Option<Battery>, profiles: Option<(&str, &[&str])>) -> zbus::Connection {
    let mut builder = bus.builder();
    if let Some(battery) = battery {
        builder = builder
            .serve_at("/org/freedesktop/UPower", Manager(battery.on_battery))
            .unwrap()
            .serve_at("/org/freedesktop/UPower/devices/DisplayDevice", Device(battery))
            .unwrap()
            .name("org.freedesktop.UPower")
            .unwrap();
    }
    if let Some((active, all)) = profiles {
        builder = builder
            .serve_at("/org/freedesktop/UPower/PowerProfiles", Profiles { active: active.to_owned(), all: all.iter().map(|p| (*p).to_owned()).collect() })
            .unwrap()
            .name("org.freedesktop.UPower.PowerProfiles")
            .unwrap();
    }
    builder.build().await.unwrap()
}
