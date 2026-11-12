//! DisplayConfig `Backlight` / `SetBacklight` over a private bus, with a
//! fake `/sys/class/backlight` and a fake logind that writes to it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::StreamExt;
use helix_comp::backlight::Backlight;
use helix_comp::display_config::{Mode, Monitor, Store, ALL_TRANSFORMS};
use helix_comp::{CompositorServices, MonitorControl};
use helix_session::logind::Logind;
use helix_test_support::logind::FakeLogind;
use helix_test_support::TestBus;
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedValue;

type Props = HashMap<String, OwnedValue>;

#[zbus::proxy(
    interface = "org.gnome.Mutter.DisplayConfig",
    default_service = "org.gnome.Mutter.DisplayConfig",
    default_path = "/org/gnome/Mutter/DisplayConfig"
)]
trait DisplayConfig {
    fn set_backlight(&self, serial: u32, connector: &str, value: i32) -> zbus::Result<()>;
    #[zbus(property)]
    fn backlight(&self) -> zbus::Result<(u32, Vec<Props>)>;
}

fn monitor(connector: &str, builtin: bool) -> Monitor {
    Monitor {
        connector: connector.into(),
        vendor: "Acme".into(),
        product: "Panel".into(),
        serial: connector.into(),
        display_name: connector.into(),
        builtin,
        modes: vec![Mode { width: 1920, height: 1080, refresh_mhz: 60_000, preferred: true, current: true }],
        x: if builtin { 0 } else { 1920 },
        y: 0,
        scale: 1.0,
        transform: 0,
        primary: builtin,
        transforms: ALL_TRANSFORMS.to_vec(),
        hdr: false,
        color_mode: 0,
    }
}

fn sysfs(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("helix-backlight-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dev = root.join("intel_backlight");
    std::fs::create_dir_all(&dev).unwrap();
    std::fs::write(dev.join("type"), "raw\n").unwrap();
    std::fs::write(dev.join("max_brightness"), "1000\n").unwrap();
    std::fs::write(dev.join("brightness"), "400\n").unwrap();
    root
}

fn get<T: TryFrom<OwnedValue>>(entry: &Props, key: &str) -> Option<T> {
    entry.get(key).and_then(|v| T::try_from(v.clone()).ok())
}

#[tokio::test]
async fn backlight_of_the_builtin_panel() {
    let Some(bus) = TestBus::start() else { return };
    let logind = FakeLogind::start(&bus).await;
    let root = sysfs("panel");
    logind.write_brightness_to(root.clone());
    let backlight = Backlight::new(root.clone(), Some(Logind::new(bus.connect().await)));
    let control = MonitorControl { store: Store::at(root.join("monitors.toml")), backlight: Some(backlight), ..MonitorControl::default() };
    let monitors = vec![monitor("eDP-1", true), monitor("HDMI-A-1", false)];
    let services = CompositorServices::start_with(bus.builder(), monitors, control).await.unwrap();
    let (_events, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(services.run(rx));

    let conn = bus.connect().await;
    let dc = DisplayConfigProxy::builder(&conn).cache_properties(CacheProperties::No).build().await.unwrap();
    let (serial, entries) = dc.backlight().await.unwrap();
    assert_eq!(entries.len(), 2, "one entry per monitor");
    let panel = &entries[0];
    assert_eq!(get::<String>(panel, "connector").as_deref(), Some("eDP-1"));
    assert_eq!(get::<bool>(panel, "active"), Some(true));
    assert_eq!((get::<i32>(panel, "min"), get::<i32>(panel, "max"), get::<i32>(panel, "value")), (Some(1), Some(1000), Some(400)));
    let external = &entries[1];
    assert_eq!(get::<String>(external, "connector").as_deref(), Some("HDMI-A-1"));
    assert!(get::<i32>(external, "value").is_none(), "no control for external monitors");

    // Set through logind, then published.
    let watcher = DisplayConfigProxy::builder(&conn).build().await.unwrap();
    let mut backlight_changes = watcher.receive_backlight_changed().await;
    dc.set_backlight(serial, "eDP-1", 250).await.unwrap();
    assert!(logind.calls().contains(&"SetBrightness backlight intel_backlight 250".to_owned()));
    let (_, entries) = dc.backlight().await.unwrap();
    assert_eq!(get::<i32>(&entries[0], "value"), Some(250));
    let changed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let change = backlight_changes.next().await.unwrap();
            let (_, entries) = change.get().await.unwrap();
            if get::<i32>(&entries[0], "value") == Some(250) {
                break;
            }
        }
    })
    .await;
    assert!(changed.is_ok(), "Backlight change emitted");

    // Refused: stale serial, uncontrollable monitor, out of range.
    assert!(dc.set_backlight(serial + 1, "eDP-1", 300).await.is_err());
    assert!(dc.set_backlight(serial, "HDMI-A-1", 300).await.is_err());
    assert!(dc.set_backlight(serial, "eDP-1", 0).await.is_err());
    assert!(dc.set_backlight(serial, "eDP-1", 1001).await.is_err());

    // Changed elsewhere: noticed by polling.
    std::fs::write(root.join("intel_backlight/brightness"), "900\n").unwrap();
    let noticed = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let change = backlight_changes.next().await.unwrap();
            let (_, entries) = change.get().await.unwrap();
            if get::<i32>(&entries[0], "value") == Some(900) {
                break;
            }
        }
    })
    .await;
    assert!(noticed.is_ok(), "outside change published");
    std::fs::remove_dir_all(&root).unwrap();
}

/// An external monitor on a simulated DDC/CI bus (no real monitor is used).
#[tokio::test]
async fn backlight_of_an_external_monitor() {
    use helix_comp::ddc::simulated::{Fake, Monitor as Simulated};
    use helix_comp::ddc::{Bus, Ddc};
    use std::sync::{Arc, Mutex};

    let Some(bus) = TestBus::start() else { return };
    let root = sysfs("ddc");
    let drm = root.join("drm");
    std::fs::create_dir_all(drm.join("card1-HDMI-A-1")).unwrap();
    std::os::unix::fs::symlink("../../../i2c-4", drm.join("card1-HDMI-A-1/ddc")).unwrap();
    let screen = Arc::new(Mutex::new(Simulated { brightness: Some((100, 75)), ..Default::default() }));
    let sim = screen.clone();
    let ddc = Ddc::new(drm, root.join("dev"), Box::new(move |_| Ok(Box::new(Fake(sim.clone())) as Box<dyn Bus>)));
    let backlight = Backlight::new(root.join("none"), None).with_ddc(ddc);
    let control = MonitorControl { store: Store::at(root.join("monitors.toml")), backlight: Some(backlight), ..MonitorControl::default() };
    let services = CompositorServices::start_with(bus.builder(), vec![monitor("HDMI-A-1", false)], control).await.unwrap();
    let (_events, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(services.run(rx));

    let conn = bus.connect().await;
    let dc = DisplayConfigProxy::builder(&conn).cache_properties(CacheProperties::No).build().await.unwrap();
    // Found by the first poll.
    let found = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let (serial, entries) = dc.backlight().await.unwrap();
            if get::<i32>(&entries[0], "value").is_some() {
                return (serial, entries);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    let (serial, entries) = found.expect("the monitor's DDC/CI brightness is offered");
    assert_eq!((get::<i32>(&entries[0], "min"), get::<i32>(&entries[0], "max"), get::<i32>(&entries[0], "value")), (Some(0), Some(100), Some(75)));

    dc.set_backlight(serial, "HDMI-A-1", 40).await.unwrap();
    assert_eq!(screen.lock().unwrap().brightness, Some((100, 40)), "written to the monitor");
    let (_, entries) = dc.backlight().await.unwrap();
    assert_eq!(get::<i32>(&entries[0], "value"), Some(40));
    assert!(dc.set_backlight(serial, "HDMI-A-1", 101).await.is_err());
    std::fs::remove_dir_all(&root).unwrap();
}
