//! IdleMonitor and DisplayConfig over a private bus, driven by synthetic
//! compositor events.

use std::time::Duration;

use futures_util::StreamExt;
use helix_comp::display_config::{Mode, Monitor};
use helix_comp::{CompositorServices, Event};
use helix_test_support::TestBus;
use tokio::sync::mpsc;
use zbus::proxy::CacheProperties;

#[zbus::proxy(
    interface = "org.gnome.Mutter.IdleMonitor",
    default_service = "org.gnome.Mutter.IdleMonitor",
    default_path = "/org/gnome/Mutter/IdleMonitor/Core"
)]
trait IdleMonitor {
    fn get_idletime(&self) -> zbus::Result<u64>;
    fn add_idle_watch(&self, interval: u64) -> zbus::Result<u32>;
    fn add_user_active_watch(&self) -> zbus::Result<u32>;
    fn remove_watch(&self, id: u32) -> zbus::Result<()>;
    fn reset_idletime(&self) -> zbus::Result<()>;
    #[zbus(signal)]
    fn watch_fired(&self, id: u32) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.gnome.Mutter.DisplayConfig",
    default_service = "org.gnome.Mutter.DisplayConfig",
    default_path = "/org/gnome/Mutter/DisplayConfig"
)]
trait DisplayConfig {
    #[allow(clippy::type_complexity)]
    fn get_current_state(
        &self,
    ) -> zbus::Result<(
        u32,
        Vec<(
            (String, String, String, String),
            Vec<(String, i32, i32, f64, f64, Vec<f64>, std::collections::HashMap<String, zbus::zvariant::OwnedValue>)>,
            std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
        )>,
        Vec<(i32, i32, f64, u32, bool, Vec<(String, String, String, String)>, std::collections::HashMap<String, zbus::zvariant::OwnedValue>)>,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    )>;
    #[zbus(signal)]
    fn monitors_changed(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn has_external_monitor(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn power_save_mode(&self) -> zbus::Result<i32>;
    #[zbus(property)]
    fn set_power_save_mode(&self, mode: i32) -> zbus::Result<()>;
}

fn monitor(width: i32, builtin: bool) -> Monitor {
    Monitor {
        connector: "WINIT-1".into(),
        vendor: "Helix".into(),
        product: "Nested".into(),
        serial: "0".into(),
        display_name: "Nested window".into(),
        builtin,
        modes: vec![Mode { width, height: 600, refresh_mhz: 60_000, preferred: true, current: true }],
        x: 0,
        y: 0,
        scale: 1.0,
        transform: 0,
        primary: true,
        transforms: vec![0],
        hdr: false,
        color_mode: 0,
    }
}

async fn start(bus: &TestBus) -> mpsc::UnboundedSender<Event> {
    let services = CompositorServices::start(bus.builder(), vec![monitor(800, true)]).await.unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(services.run(rx));
    tx
}

async fn next<T>(stream: &mut (impl futures_util::Stream<Item = T> + Unpin)) -> T {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("signal")
        .expect("stream open")
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_and_user_active_watches() {
    let Some(bus) = TestBus::start() else { return };
    let events = start(&bus).await;
    let conn = bus.connect().await;
    let idle = IdleMonitorProxy::new(&conn).await.unwrap();
    let mut fired = idle.receive_watch_fired().await.unwrap();

    let watch = idle.add_idle_watch(150).await.unwrap();
    assert_eq!(next(&mut fired).await.args().unwrap().id, watch);
    assert!(idle.get_idletime().await.unwrap() >= 150);

    let active = idle.add_user_active_watch().await.unwrap();
    events.send(Event::Activity).unwrap();
    assert_eq!(next(&mut fired).await.args().unwrap().id, active);
    assert!(idle.get_idletime().await.unwrap() < 150);
    // Activity re-arms the idle watch.
    assert_eq!(next(&mut fired).await.args().unwrap().id, watch);

    idle.remove_watch(watch).await.unwrap();
    assert!(idle.remove_watch(watch).await.is_err());
    assert!(idle.add_idle_watch(0).await.is_err());

    let active = idle.add_user_active_watch().await.unwrap();
    idle.reset_idletime().await.unwrap();
    assert_eq!(next(&mut fired).await.args().unwrap().id, active);
}

#[tokio::test(flavor = "multi_thread")]
async fn object_manager_lists_core() {
    let Some(bus) = TestBus::start() else { return };
    let _events = start(&bus).await;
    let conn = bus.connect().await;
    let om = zbus::fdo::ObjectManagerProxy::builder(&conn)
        .destination("org.gnome.Mutter.IdleMonitor").unwrap()
        .path("/org/gnome/Mutter/IdleMonitor").unwrap()
        .build().await.unwrap();
    let objects = om.get_managed_objects().await.unwrap();
    let core = objects
        .iter()
        .find(|(p, _)| p.as_str() == "/org/gnome/Mutter/IdleMonitor/Core")
        .expect("Core object");
    assert!(core.1.keys().any(|i| i.as_str() == "org.gnome.Mutter.IdleMonitor"));
}

#[tokio::test(flavor = "multi_thread")]
async fn display_config_tracks_monitors() {
    let Some(bus) = TestBus::start() else { return };
    let events = start(&bus).await;
    let conn = bus.connect().await;
    let dc = DisplayConfigProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build().await.unwrap();
    let mut changed = dc.receive_monitors_changed().await.unwrap();

    let (serial, monitors, logical, _) = dc.get_current_state().await.unwrap();
    assert_eq!(monitors[0].1[0].1, 800);
    assert_eq!(logical.len(), 1);
    assert!(!dc.has_external_monitor().await.unwrap());

    events.send(Event::Monitors(vec![monitor(1024, false)])).unwrap();
    next(&mut changed).await;
    let (new_serial, monitors, _, _) = dc.get_current_state().await.unwrap();
    assert_ne!(new_serial, serial);
    assert_eq!(monitors[0].1[0].1, 1024);
    assert!(dc.has_external_monitor().await.unwrap());

    assert_eq!(dc.power_save_mode().await.unwrap(), 0);
    dc.set_power_save_mode(3).await.unwrap();
    assert_eq!(dc.power_save_mode().await.unwrap(), 3);
    assert!(dc.set_power_save_mode(9).await.is_err());
}
