//! System status for the panel and quick settings (helix-desktop-spec
//! `contracts/wayland/README.md`, "Quick settings", "System status"):
//! network, battery, volume, Wi-Fi, Bluetooth, airplane mode and power
//! profiles, from the system bus.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use futures_util::StreamExt;
use gio::prelude::*;
use tokio::sync::{mpsc, Notify};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::Connection;

use crate::UiEvent;

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const UPOWER: &str = "org.freedesktop.UPower";
const BLUEZ: &str = "org.bluez";
const PROFILES: [(&str, &str); 2] = [("org.freedesktop.UPower.PowerProfiles", "org.freedesktop.UPower.PowerProfiles"), ("net.hadess.PowerProfiles", "net.hadess.PowerProfiles")];
const PROFILES_PATH: &str = "/org/freedesktop/UPower/PowerProfiles";
const REFRESH: Duration = Duration::from_secs(5);

/// What the panel and quick settings show.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    /// The network icon, when NetworkManager runs.
    pub network: Option<String>,
    /// Wi-Fi: (enabled, connected network's name), when NetworkManager runs.
    pub wifi: Option<(bool, Option<String>)>,
    /// Bluetooth powered, when an adapter exists.
    pub bluetooth: Option<bool>,
    pub airplane: bool,
    /// Power profiles offered, and the active one.
    pub profiles: Vec<String>,
    pub profile: Option<String>,
    /// Battery: (icon, percent), when present.
    pub battery: Option<(String, f64)>,
    pub show_percentage: bool,
    /// The volume icon, when a sound server answers.
    pub volume: Option<String>,
    /// iio-sensor-proxy has an accelerometer (automatic rotation).
    pub accelerometer: bool,
    /// An app uses the location (GeoClue), with location services on.
    pub location: bool,
    /// An app records from a microphone; a camera is running.
    pub microphone: bool,
    pub camera: bool,
    /// Wi-Fi networks in reach (while Wi-Fi is on), strongest first.
    pub networks: Vec<Network>,
    /// Paired Bluetooth devices (while Bluetooth is on).
    pub devices: Vec<BtDevice>,
}

/// A Wi-Fi network in reach.
#[derive(Debug, Clone, PartialEq)]
pub struct Network {
    pub ssid: String,
    pub strength: u8,
    pub secured: bool,
    pub active: bool,
}

/// A paired Bluetooth device.
#[derive(Debug, Clone, PartialEq)]
pub struct BtDevice {
    pub path: String,
    pub name: String,
    pub connected: bool,
}

/// At most this many networks or devices are listed.
pub const LISTED: usize = 8;

/// One entry per name (the strongest; connected if any is), strongest
/// first; nameless networks are left out.
pub fn group_networks(access_points: Vec<Network>) -> Vec<Network> {
    let mut out: Vec<Network> = Vec::new();
    for ap in access_points.into_iter().filter(|a| !a.ssid.is_empty()) {
        match out.iter_mut().find(|n| n.ssid == ap.ssid) {
            Some(n) => {
                n.active |= ap.active;
                if ap.strength > n.strength {
                    n.strength = ap.strength;
                    n.secured = ap.secured;
                }
            }
            None => out.push(ap),
        }
    }
    out.sort_by(|a, b| b.active.cmp(&a.active).then(b.strength.cmp(&a.strength)).then_with(|| a.ssid.cmp(&b.ssid)));
    out.truncate(LISTED);
    out
}

/// An access point needs a password: the privacy flag, or WPA/RSN flags.
pub fn is_secured(flags: u32, wpa: u32, rsn: u32) -> bool {
    flags & 1 != 0 || wpa != 0 || rsn != 0
}

/// A quick settings switch acting on the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Toggle {
    Wifi(bool),
    Bluetooth(bool),
    Airplane(bool),
    Profile(String),
    /// Connect to the Wi-Fi network of this name.
    ConnectWifi(String),
    /// Connect (true) or disconnect a Bluetooth device, by object path.
    BluetoothDevice { path: String, connect: bool },
}

/// The Wi-Fi icon for an access point's strength (0–100).
pub fn wifi_icon(strength: u8) -> &'static str {
    match strength {
        0..20 => "network-wireless-signal-none-symbolic",
        20..40 => "network-wireless-signal-weak-symbolic",
        40..50 => "network-wireless-signal-ok-symbolic",
        50..80 => "network-wireless-signal-good-symbolic",
        _ => "network-wireless-signal-excellent-symbolic",
    }
}

/// The network icon: `primary` the active connection's type (and Wi-Fi
/// strength), if any.
pub fn network_icon(airplane: bool, wifi_on: bool, primary: Option<(&str, Option<u8>)>) -> String {
    if airplane {
        return "airplane-mode-symbolic".into();
    }
    match primary {
        Some(("802-11-wireless", strength)) => wifi_icon(strength.unwrap_or(0)).into(),
        Some(("vpn" | "wireguard", _)) => "network-vpn-symbolic".into(),
        Some(("gsm" | "cdma", _)) => "network-cellular-connected-symbolic".into(),
        Some(_) => "network-wired-symbolic".into(),
        None if wifi_on => "network-wireless-offline-symbolic".into(),
        None => "network-offline-symbolic".into(),
    }
}

/// Airplane mode: no radio on.
pub fn airplane(wifi_on: Option<bool>, bluetooth_on: Option<bool>) -> bool {
    (wifi_on.is_some() || bluetooth_on.is_some()) && wifi_on != Some(true) && bluetooth_on != Some(true)
}

async fn prop<T: TryFrom<OwnedValue>>(conn: &Connection, dest: &str, path: &str, iface: &str, name: &str) -> Option<T>
where
    T::Error: Into<zbus::Error>,
{
    let proxy = zbus::Proxy::new_owned(conn.clone(), dest.to_owned(), path.to_owned(), iface.to_owned()).await.ok()?;
    proxy.get_property(name).await.ok()
}

async fn set_prop(conn: &Connection, dest: &str, path: &str, iface: &str, name: &str, value: Value<'_>) -> zbus::Result<()> {
    conn.call_method(Some(dest), path, Some("org.freedesktop.DBus.Properties"), "Set", &(iface, name, value)).await.map(drop)
}

type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

async fn bluez_objects(conn: &Connection) -> Objects {
    match conn.call_method(Some(BLUEZ), "/", Some("org.freedesktop.DBus.ObjectManager"), "GetManagedObjects", &()).await {
        Ok(reply) => reply.body().deserialize::<Objects>().unwrap_or_default(),
        Err(_) => Objects::new(),
    }
}

/// BlueZ's paired devices, by name.
async fn bt_devices(conn: &Connection) -> Vec<BtDevice> {
    let get = |props: &HashMap<String, OwnedValue>, key: &str| props.get(key).cloned();
    let mut out: Vec<BtDevice> = bluez_objects(conn)
        .await
        .into_iter()
        .filter_map(|(path, ifaces)| {
            let d = ifaces.get("org.bluez.Device1")?;
            let paired = get(d, "Paired").and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
            let name = get(d, "Alias").or_else(|| get(d, "Name")).and_then(|v| String::try_from(v).ok()).unwrap_or_default();
            let connected = get(d, "Connected").and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
            paired.then(|| BtDevice { path: path.to_string(), name, connected })
        })
        .collect();
    out.sort_by(|a, b| b.connected.cmp(&a.connected).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    out.truncate(LISTED);
    out
}

const NM_WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const NM_AP: &str = "org.freedesktop.NetworkManager.AccessPoint";

/// NetworkManager's Wi-Fi devices.
async fn wifi_devices(conn: &Connection) -> Vec<OwnedObjectPath> {
    let devices: Vec<OwnedObjectPath> = match conn.call_method(Some(NM), NM_PATH, Some(NM), "GetDevices", &()).await {
        Ok(reply) => reply.body().deserialize().unwrap_or_default(),
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for d in devices {
        if prop::<u32>(conn, NM, d.as_str(), "org.freedesktop.NetworkManager.Device", "DeviceType").await == Some(2) {
            out.push(d);
        }
    }
    out
}

/// Every access point in reach: (device, access point path, network).
async fn access_points(conn: &Connection) -> Vec<(OwnedObjectPath, OwnedObjectPath, Network)> {
    let mut out = Vec::new();
    for device in wifi_devices(conn).await {
        let active = prop::<OwnedObjectPath>(conn, NM, device.as_str(), NM_WIRELESS, "ActiveAccessPoint").await;
        let aps = prop::<Vec<OwnedObjectPath>>(conn, NM, device.as_str(), NM_WIRELESS, "AccessPoints").await.unwrap_or_default();
        for ap in aps {
            let Ok(reply) = conn.call_method(Some(NM), ap.as_str(), Some("org.freedesktop.DBus.Properties"), "GetAll", &(NM_AP,)).await else { continue };
            let Ok(props) = reply.body().deserialize::<HashMap<String, OwnedValue>>() else { continue };
            let num = |k: &str| props.get(k).and_then(|v| u32::try_from(v.clone()).ok()).unwrap_or(0);
            let ssid = props.get("Ssid").and_then(|v| Vec::<u8>::try_from(v.clone()).ok()).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
            let strength = props.get("Strength").and_then(|v| u8::try_from(v.clone()).ok()).unwrap_or(0);
            let network = Network { ssid, strength, secured: is_secured(num("Flags"), num("WpaFlags"), num("RsnFlags")), active: active.as_ref() == Some(&ap) };
            out.push((device.clone(), ap, network));
        }
    }
    out
}

/// A saved connection for the network `ssid`.
async fn saved_connection(conn: &Connection, ssid: &str) -> Option<OwnedObjectPath> {
    const SETTINGS: &str = "org.freedesktop.NetworkManager.Settings";
    let reply = conn.call_method(Some(NM), "/org/freedesktop/NetworkManager/Settings", Some(SETTINGS), "ListConnections", &()).await.ok()?;
    let list: Vec<OwnedObjectPath> = reply.body().deserialize().ok()?;
    for path in list {
        let Ok(reply) = conn.call_method(Some(NM), path.as_str(), Some("org.freedesktop.NetworkManager.Settings.Connection"), "GetSettings", &()).await else { continue };
        let Ok(settings) = reply.body().deserialize::<HashMap<String, HashMap<String, OwnedValue>>>() else { continue };
        let saved = settings.get("802-11-wireless").and_then(|w| w.get("ssid")).and_then(|v| Vec::<u8>::try_from(v.clone()).ok());
        if saved.as_deref() == Some(ssid.as_bytes()) {
            return Some(path);
        }
    }
    None
}

/// Connect to the network `ssid` (see the spec's "Wi-Fi networks and
/// Bluetooth devices").
async fn connect_wifi(conn: &Connection, ssid: &str) -> zbus::Result<()> {
    let best = access_points(conn).await.into_iter().filter(|(_, _, n)| n.ssid == ssid).max_by_key(|(_, _, n)| n.strength);
    let Some((device, ap, network)) = best else { return Err(zbus::Error::Failure(format!("no network {ssid}"))) };
    if network.active {
        return Ok(());
    }
    if let Some(saved) = saved_connection(conn, ssid).await {
        tracing::info!(ssid, "activating the saved Wi-Fi connection");
        return conn.call_method(Some(NM), NM_PATH, Some(NM), "ActivateConnection", &(saved, device, ap)).await.map(drop);
    }
    if !network.secured {
        tracing::info!(ssid, "joining an open Wi-Fi network");
        let empty: HashMap<&str, HashMap<&str, Value<'_>>> = HashMap::new();
        return conn.call_method(Some(NM), NM_PATH, Some(NM), "AddAndActivateConnection", &(empty, device, ap)).await.map(drop);
    }
    tracing::info!(ssid, "a new secured network: opening the Wi-Fi settings");
    if let Err(err) = std::process::Command::new("gnome-control-center").arg("wifi").spawn() {
        tracing::warn!(%err, "cannot open the Wi-Fi settings");
    }
    Ok(())
}

/// BlueZ adapters: (path, powered).
async fn adapters(conn: &Connection) -> Vec<(OwnedObjectPath, bool)> {
    let objects = bluez_objects(conn).await;
    let mut out: Vec<(OwnedObjectPath, bool)> = objects
        .into_iter()
        .filter_map(|(path, ifaces)| {
            let adapter = ifaces.get("org.bluez.Adapter1")?;
            let powered = adapter.get("Powered").and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(false);
            Some((path, powered))
        })
        .collect();
    out.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    out
}

/// The power-profiles service: (bus name and interface).
async fn profiles_service(conn: &Connection) -> Option<(&'static str, &'static str)> {
    for (name, iface) in PROFILES {
        if prop::<String>(conn, name, PROFILES_PATH, iface, "ActiveProfile").await.is_some() {
            return Some((name, iface));
        }
    }
    None
}

/// Read everything now.
fn location_enabled() -> bool {
    helix_settings::has_schema(crate::location::LOCATION) && gio::Settings::new(crate::location::LOCATION).boolean("enabled")
}

pub async fn read(system: Option<&Connection>) -> Status {
    let (microphone, camera) = crate::privacy::current();
    let mut status = Status { show_percentage: show_percentage(), microphone, camera, ..Status::default() };
    if let Some((percent, muted)) = crate::shell_keys::volume_state().await {
        status.volume = Some(crate::shell_keys::volume_icon(percent, muted).to_owned());
    }
    let Some(conn) = system else { return status };
    // NetworkManager.
    let wifi_on = prop::<bool>(conn, NM, NM_PATH, NM, "WirelessEnabled").await;
    let mut primary: Option<(String, Option<u8>, Option<String>)> = None;
    if wifi_on.is_some() {
        if let Some(active) = prop::<OwnedObjectPath>(conn, NM, NM_PATH, NM, "PrimaryConnection").await.filter(|p| p.as_str() != "/") {
            let iface = "org.freedesktop.NetworkManager.Connection.Active";
            let kind = prop::<String>(conn, NM, active.as_str(), iface, "Type").await.unwrap_or_default();
            let (mut strength, mut ssid) = (None, None);
            if kind == "802-11-wireless" {
                if let Some(ap) = prop::<OwnedObjectPath>(conn, NM, active.as_str(), iface, "SpecificObject").await.filter(|p| p.as_str() != "/") {
                    let ap_iface = "org.freedesktop.NetworkManager.AccessPoint";
                    strength = prop::<u8>(conn, NM, ap.as_str(), ap_iface, "Strength").await;
                    ssid = prop::<Vec<u8>>(conn, NM, ap.as_str(), ap_iface, "Ssid").await.map(|b| String::from_utf8_lossy(&b).into_owned());
                }
            }
            primary = Some((kind, strength, ssid));
        }
    }
    // BlueZ.
    let adapters = adapters(conn).await;
    let bluetooth = (!adapters.is_empty()).then(|| adapters.iter().any(|(_, on)| *on));
    status.bluetooth = bluetooth;
    status.airplane = airplane(wifi_on, bluetooth);
    if bluetooth == Some(true) {
        status.devices = bt_devices(conn).await;
    }
    if wifi_on == Some(true) {
        status.networks = group_networks(access_points(conn).await.into_iter().map(|(_, _, n)| n).collect());
    }
    if let Some(on) = wifi_on {
        let ssid = primary.as_ref().filter(|p| p.0 == "802-11-wireless").and_then(|p| p.2.clone());
        status.wifi = Some((on, ssid));
        status.network = Some(network_icon(status.airplane, on, primary.as_ref().map(|p| (p.0.as_str(), p.1))));
    }
    // GeoClue, only while it runs.
    let geoclue = "org.freedesktop.GeoClue2";
    if location_enabled() {
        let running = match zbus::fdo::DBusProxy::new(conn).await {
            Ok(dbus) => dbus.name_has_owner(zbus::names::BusName::from_static_str(geoclue).expect("a valid name")).await.unwrap_or(false),
            Err(_) => false,
        };
        if running {
            status.location = prop::<bool>(conn, geoclue, "/org/freedesktop/GeoClue2/Manager", "org.freedesktop.GeoClue2.Manager", "InUse").await == Some(true);
        }
    }
    // UPower.
    let display = "/org/freedesktop/UPower/devices/DisplayDevice";
    let device = "org.freedesktop.UPower.Device";
    if prop::<bool>(conn, UPOWER, display, device, "IsPresent").await == Some(true) {
        let percent = prop::<f64>(conn, UPOWER, display, device, "Percentage").await.unwrap_or(0.0);
        let icon = prop::<String>(conn, UPOWER, display, device, "IconName").await.filter(|i| !i.is_empty()).unwrap_or_else(|| "battery-symbolic".into());
        status.battery = Some((icon, percent));
    }
    // The accelerometer.
    status.accelerometer = prop::<bool>(conn, "net.hadess.SensorProxy", "/net/hadess/SensorProxy", "net.hadess.SensorProxy", "HasAccelerometer").await == Some(true);
    // Power profiles.
    if let Some((name, iface)) = profiles_service(conn).await {
        status.profile = prop::<String>(conn, name, PROFILES_PATH, iface, "ActiveProfile").await;
        if let Some(list) = prop::<Vec<HashMap<String, OwnedValue>>>(conn, name, PROFILES_PATH, iface, "Profiles").await {
            status.profiles = list.iter().filter_map(|p| p.get("Profile").and_then(|v| String::try_from(v.clone()).ok())).collect();
        }
    }
    status
}

fn show_percentage() -> bool {
    let Some(source) = gio::SettingsSchemaSource::default() else { return false };
    let Some(schema) = source.lookup("org.gnome.desktop.interface", true) else { return false };
    schema.has_key("show-battery-percentage") && gio::Settings::new("org.gnome.desktop.interface").boolean("show-battery-percentage")
}

/// Turn a switch on or off.
pub async fn apply(system: &Connection, toggle: Toggle) -> zbus::Result<()> {
    match toggle {
        Toggle::Wifi(on) => set_prop(system, NM, NM_PATH, NM, "WirelessEnabled", Value::from(on)).await,
        Toggle::Bluetooth(on) => {
            for (path, _) in adapters(system).await {
                set_prop(system, BLUEZ, path.as_str(), "org.bluez.Adapter1", "Powered", Value::from(on)).await?;
            }
            Ok(())
        }
        Toggle::Airplane(on) => {
            let radios = !on;
            if prop::<bool>(system, NM, NM_PATH, NM, "WirelessEnabled").await.is_some() {
                set_prop(system, NM, NM_PATH, NM, "WirelessEnabled", Value::from(radios)).await?;
                if on {
                    let _ = set_prop(system, NM, NM_PATH, NM, "WwanEnabled", Value::from(false)).await;
                }
            }
            for (path, _) in adapters(system).await {
                set_prop(system, BLUEZ, path.as_str(), "org.bluez.Adapter1", "Powered", Value::from(radios)).await?;
            }
            Ok(())
        }
        Toggle::Profile(profile) => {
            let (name, iface) = profiles_service(system).await.ok_or_else(|| zbus::Error::Failure("no power profiles".into()))?;
            set_prop(system, name, PROFILES_PATH, iface, "ActiveProfile", Value::from(profile.as_str())).await
        }
        Toggle::ConnectWifi(ssid) => connect_wifi(system, &ssid).await,
        Toggle::BluetoothDevice { path, connect } => {
            let method = if connect { "Connect" } else { "Disconnect" };
            system.call_method(Some(BLUEZ), path.as_str(), Some("org.bluez.Device1"), method, &()).await.map(drop)
        }
    }
}

static POKE: LazyLock<Notify> = LazyLock::new(Notify::new);

/// Something changed (volume, a switch): read the status again soon.
pub fn changed() {
    POKE.notify_one();
}

/// Keep the UI's status current: on property changes on the system bus,
/// [`changed`], and every few seconds.
pub async fn follow(ui: mpsc::UnboundedSender<UiEvent>) {
    let system = Connection::system().await.ok();
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.DBus.Properties")
        .and_then(|b| b.member("PropertiesChanged"))
        .map(|b| b.build());
    let mut signals = match (&system, rule) {
        (Some(conn), Ok(rule)) => zbus::MessageStream::for_match_rule(rule, conn, Some(64)).await.ok(),
        _ => None,
    };
    let mut last: Option<Status> = None;
    loop {
        let status = read(system.as_ref()).await;
        if last.as_ref() != Some(&status) {
            let _ = ui.send(UiEvent::Status(Box::new(status.clone())));
            last = Some(status);
        }
        let signal = async {
            match signals.as_mut() {
                Some(s) => {
                    s.next().await;
                }
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = signal => {}
            _ = POKE.notified() => {}
            _ = tokio::time::sleep(REFRESH) => {}
        }
        // Changes come in bursts.
        tokio::time::sleep(Duration::from_millis(150)).await;
        if let Some(s) = signals.as_mut() {
            while futures_util::FutureExt::now_or_never(s.next()).flatten().is_some() {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_lists() {
        let n = |ssid: &str, strength, secured, active| Network { ssid: ssid.into(), strength, secured, active };
        let grouped = group_networks(vec![n("Cafe", 40, false, false), n("Home", 30, true, true), n("Cafe", 70, false, false), n("", 99, false, false), n("Home", 60, true, false)]);
        assert_eq!(grouped, [n("Home", 60, true, true), n("Cafe", 70, false, false)], "connected first, strongest kept, nameless dropped");
        assert!(is_secured(1, 0, 0) && is_secured(0, 0, 0x188) && !is_secured(0, 0, 0));
    }

    #[test]
    fn icons() {
        assert_eq!(wifi_icon(10), "network-wireless-signal-none-symbolic");
        assert_eq!(wifi_icon(45), "network-wireless-signal-ok-symbolic");
        assert_eq!(wifi_icon(90), "network-wireless-signal-excellent-symbolic");
        assert_eq!(network_icon(false, true, Some(("802-3-ethernet", None))), "network-wired-symbolic");
        assert_eq!(network_icon(false, true, Some(("802-11-wireless", Some(60)))), "network-wireless-signal-good-symbolic");
        assert_eq!(network_icon(false, true, None), "network-wireless-offline-symbolic");
        assert_eq!(network_icon(false, false, None), "network-offline-symbolic");
        assert_eq!(network_icon(true, false, None), "airplane-mode-symbolic");
        assert_eq!(network_icon(false, true, Some(("wireguard", None))), "network-vpn-symbolic");
    }

    #[test]
    fn airplane_mode() {
        assert!(!airplane(None, None), "no radios at all");
        assert!(airplane(Some(false), None));
        assert!(airplane(Some(false), Some(false)));
        assert!(!airplane(Some(true), Some(false)));
        assert!(!airplane(None, Some(true)));
    }
}
