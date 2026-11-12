//! System status from fake NetworkManager, BlueZ, UPower and
//! power-profiles-daemon services (helix-desktop-spec
//! `contracts/wayland/README.md`, "Quick settings", "System status").

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use helix_shell::status::{self, Toggle};
use helix_test_support::TestBus;
use zbus::interface;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

#[derive(Default)]
struct Radios {
    wifi: bool,
    wwan: bool,
    bluetooth: bool,
    profile: String,
}

type Shared = Arc<Mutex<Radios>>;

struct Nm(Shared);

#[interface(name = "org.freedesktop.NetworkManager")]
impl Nm {
    #[zbus(property)]
    fn wireless_enabled(&self) -> bool {
        self.0.lock().unwrap().wifi
    }
    #[zbus(property)]
    fn set_wireless_enabled(&mut self, on: bool) {
        self.0.lock().unwrap().wifi = on;
    }
    #[zbus(property)]
    fn wwan_enabled(&self) -> bool {
        self.0.lock().unwrap().wwan
    }
    #[zbus(property)]
    fn set_wwan_enabled(&mut self, on: bool) {
        self.0.lock().unwrap().wwan = on;
    }
    #[zbus(property)]
    fn primary_connection(&self) -> OwnedObjectPath {
        let path = if self.0.lock().unwrap().wifi { "/org/freedesktop/NetworkManager/ActiveConnection/1" } else { "/" };
        ObjectPath::try_from(path).unwrap().into()
    }
}

struct Active;

#[interface(name = "org.freedesktop.NetworkManager.Connection.Active")]
impl Active {
    #[zbus(property, name = "Type")]
    fn kind(&self) -> String {
        "802-11-wireless".into()
    }
    #[zbus(property)]
    fn specific_object(&self) -> OwnedObjectPath {
        ObjectPath::try_from("/org/freedesktop/NetworkManager/AccessPoint/1").unwrap().into()
    }
}

struct AccessPoint;

#[interface(name = "org.freedesktop.NetworkManager.AccessPoint")]
impl AccessPoint {
    #[zbus(property)]
    fn strength(&self) -> u8 {
        70
    }
    #[zbus(property)]
    fn ssid(&self) -> Vec<u8> {
        b"home".to_vec()
    }
}

struct ObjectManager(Shared);

#[interface(name = "org.freedesktop.DBus.ObjectManager")]
impl ObjectManager {
    fn get_managed_objects(&self) -> HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> {
        let powered = OwnedValue::from(self.0.lock().unwrap().bluetooth);
        let adapter = HashMap::from([("org.bluez.Adapter1".to_owned(), HashMap::from([("Powered".to_owned(), powered)]))]);
        HashMap::from([(ObjectPath::try_from("/org/bluez/hci0").unwrap().into(), adapter)])
    }
}

struct Adapter(Shared);

#[interface(name = "org.bluez.Adapter1")]
impl Adapter {
    #[zbus(property)]
    fn powered(&self) -> bool {
        self.0.lock().unwrap().bluetooth
    }
    #[zbus(property)]
    fn set_powered(&mut self, on: bool) {
        self.0.lock().unwrap().bluetooth = on;
    }
}

struct Battery;

#[interface(name = "org.freedesktop.UPower.Device")]
impl Battery {
    #[zbus(property)]
    fn is_present(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn percentage(&self) -> f64 {
        55.0
    }
    #[zbus(property)]
    fn icon_name(&self) -> String {
        "battery-level-50-symbolic".into()
    }
}

struct Profiles(Shared);

#[interface(name = "org.freedesktop.UPower.PowerProfiles")]
impl Profiles {
    #[zbus(property)]
    fn active_profile(&self) -> String {
        self.0.lock().unwrap().profile.clone()
    }
    #[zbus(property)]
    fn set_active_profile(&mut self, profile: String) {
        self.0.lock().unwrap().profile = profile;
    }
    #[zbus(property)]
    fn profiles(&self) -> Vec<HashMap<String, OwnedValue>> {
        ["power-saver", "balanced", "performance"].iter().map(|p| HashMap::from([("Profile".to_owned(), OwnedValue::try_from(Value::from(*p)).unwrap())])).collect()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_and_switches() {
    let Some(bus) = TestBus::start() else { return };
    let radios: Shared = Arc::new(Mutex::new(Radios { wifi: true, wwan: true, bluetooth: true, profile: "balanced".into() }));
    let _fakes = bus
        .builder()
        .serve_at("/org/freedesktop/NetworkManager", Nm(radios.clone()))
        .unwrap()
        .serve_at("/org/freedesktop/NetworkManager/ActiveConnection/1", Active)
        .unwrap()
        .serve_at("/org/freedesktop/NetworkManager/AccessPoint/1", AccessPoint)
        .unwrap()
        .serve_at("/", ObjectManager(radios.clone()))
        .unwrap()
        .serve_at("/org/bluez/hci0", Adapter(radios.clone()))
        .unwrap()
        .serve_at("/org/freedesktop/UPower/devices/DisplayDevice", Battery)
        .unwrap()
        .serve_at("/org/freedesktop/UPower/PowerProfiles", Profiles(radios.clone()))
        .unwrap()
        .name("org.freedesktop.NetworkManager")
        .unwrap()
        .name("org.bluez")
        .unwrap()
        .name("org.freedesktop.UPower")
        .unwrap()
        .name("org.freedesktop.UPower.PowerProfiles")
        .unwrap()
        .build()
        .await
        .unwrap();
    let conn = bus.connect().await;

    let s = status::read(Some(&conn)).await;
    assert_eq!(s.network.as_deref(), Some("network-wireless-signal-good-symbolic"));
    assert_eq!(s.wifi, Some((true, Some("home".into()))));
    assert_eq!(s.bluetooth, Some(true));
    assert!(!s.airplane);
    assert_eq!(s.battery, Some(("battery-level-50-symbolic".into(), 55.0)));
    assert_eq!(s.profiles, ["power-saver", "balanced", "performance"]);
    assert_eq!(s.profile.as_deref(), Some("balanced"));

    // Airplane mode: every radio off.
    status::apply(&conn, Toggle::Airplane(true)).await.unwrap();
    let s = status::read(Some(&conn)).await;
    assert!(s.airplane);
    assert_eq!(s.network.as_deref(), Some("airplane-mode-symbolic"));
    assert_eq!(s.wifi, Some((false, None)));
    assert!(!radios.lock().unwrap().wwan);

    // And back: Wi-Fi and Bluetooth on.
    status::apply(&conn, Toggle::Airplane(false)).await.unwrap();
    let s = status::read(Some(&conn)).await;
    assert!(!s.airplane);
    assert_eq!((s.wifi.map(|w| w.0), s.bluetooth), (Some(true), Some(true)));

    status::apply(&conn, Toggle::Bluetooth(false)).await.unwrap();
    status::apply(&conn, Toggle::Wifi(false)).await.unwrap();
    let s = status::read(Some(&conn)).await;
    assert_eq!((s.wifi.map(|w| w.0), s.bluetooth, s.airplane), (Some(false), Some(false), true));
    assert_eq!(s.network.as_deref(), Some("airplane-mode-symbolic"));

    status::apply(&conn, Toggle::Profile("power-saver".into())).await.unwrap();
    assert_eq!(status::read(Some(&conn)).await.profile.as_deref(), Some("power-saver"));
}
