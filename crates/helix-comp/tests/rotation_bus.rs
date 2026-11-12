//! Automatic rotation from a fake iio-sensor-proxy (helix-desktop-spec
//! `org.gnome.Mutter.DisplayConfig.md`, "Automatic rotation").

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gio::prelude::*;
use helix_comp::display_config::{ApplyRequest, Layout, Mode, Monitor, Store, ALL_TRANSFORMS};
use helix_comp::{CompositorServices, MonitorControl};
use helix_test_support::TestBus;
use tokio::sync::mpsc;
use zbus::object_server::SignalEmitter;

#[derive(Default)]
struct SensorState {
    orientation: String,
    claims: u32,
    releases: u32,
}

struct Sensor(Arc<Mutex<SensorState>>);

#[zbus::interface(name = "net.hadess.SensorProxy")]
impl Sensor {
    #[zbus(property)]
    fn has_accelerometer(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn accelerometer_orientation(&self) -> String {
        self.0.lock().unwrap().orientation.clone()
    }
    fn claim_accelerometer(&self) {
        self.0.lock().unwrap().claims += 1;
    }
    fn release_accelerometer(&self) {
        self.0.lock().unwrap().releases += 1;
    }
}

fn monitor(connector: &str, x: i32, builtin: bool) -> Monitor {
    Monitor {
        connector: connector.into(),
        vendor: "Acme".into(),
        product: "Panel".into(),
        serial: connector.into(),
        display_name: connector.into(),
        builtin,
        modes: vec![Mode { width: 1920, height: 1080, refresh_mhz: 60_000, preferred: true, current: true }],
        x,
        y: 0,
        scale: 1.0,
        transform: 0,
        primary: builtin,
        transforms: ALL_TRANSFORMS.to_vec(),
        hdr: false,
        color_mode: 0,
        underscan: None,
    }
}

fn fake_compositor(initial: Vec<Monitor>) -> mpsc::UnboundedSender<ApplyRequest> {
    let (tx, mut rx) = mpsc::unbounded_channel::<ApplyRequest>();
    tokio::spawn(async move {
        let mut current = initial;
        while let Some(request) = rx.recv().await {
            current = Layout::new(current).applied(&request.config);
            let _ = request.reply.send(Ok(current.clone()));
        }
    });
    tx
}

async fn transforms(services: &CompositorServices) -> Vec<(String, u32)> {
    let reply = services
        .connection()
        .call_method(Some("org.gnome.Mutter.DisplayConfig"), "/org/gnome/Mutter/DisplayConfig", Some("org.gnome.Mutter.DisplayConfig"), "GetCurrentState", &())
        .await
        .unwrap();
    type Props = std::collections::HashMap<String, zbus::zvariant::OwnedValue>;
    type Logical = (i32, i32, f64, u32, bool, Vec<(String, String, String, String)>, Props);
    type Spec = (String, String, String, String);
    type WireMonitor = (Spec, Vec<(String, i32, i32, f64, f64, Vec<f64>, Props)>, Props);
    let (_, _, logical, _): (u32, Vec<WireMonitor>, Vec<Logical>, Props) = reply.body().deserialize().unwrap();
    let mut out: Vec<(String, u32)> = logical.into_iter().map(|l| (l.5[0].0.clone(), l.3)).collect();
    out.sort();
    out
}

async fn set_orientation(conn: &zbus::Connection, state: &Arc<Mutex<SensorState>>, orientation: &str) {
    state.lock().unwrap().orientation = orientation.into();
    let iface = conn.object_server().interface::<_, Sensor>("/net/hadess/SensorProxy").await.unwrap();
    let emitter = SignalEmitter::new(conn, "/net/hadess/SensorProxy").unwrap();
    iface.get().await.accelerometer_orientation_changed(&emitter).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn rotates_the_builtin_panel() {
    let has_lock = gio::SettingsSchemaSource::default().and_then(|s| s.lookup(helix_comp::rotation::LOCK_SCHEMA, true)).is_some_and(|s| s.has_key("orientation-lock"));
    let Some(bus) = TestBus::start() else { return };
    if !has_lock {
        return;
    }
    // SAFETY: this test binary runs this one test.
    unsafe { std::env::set_var("GSETTINGS_BACKEND", "memory") };
    let dir = std::env::temp_dir().join(format!("helix-rotation-{}", std::process::id()));
    let monitors = vec![monitor("eDP-1", 0, true), monitor("HDMI-1", 1920, false)];
    let control = MonitorControl { applier: Some(fake_compositor(monitors.clone())), store: Store::at(dir.join("monitors.toml")), ..MonitorControl::default() };
    let services = CompositorServices::start_with(bus.builder(), monitors, control).await.unwrap();

    let state = Arc::new(Mutex::new(SensorState { orientation: "normal".into(), ..SensorState::default() }));
    let sensor = bus.builder().serve_at("/net/hadess/SensorProxy", Sensor(state.clone())).unwrap().name("net.hadess.SensorProxy").unwrap().build().await.unwrap();
    let system = bus.connect().await;
    tokio::spawn(helix_comp::rotation::run(services.connection().clone(), system));

    let wait = |want: Vec<(&'static str, u32)>| {
        let services = &services;
        async move {
            let want: Vec<(String, u32)> = want.into_iter().map(|(c, t)| (c.to_owned(), t)).collect();
            for _ in 0..100 {
                if transforms(services).await == want {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            panic!("never got {want:?}: {:?}", transforms(services).await);
        }
    };
    for _ in 0..100 {
        if state.lock().unwrap().claims > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state.lock().unwrap().claims, 1);

    set_orientation(&sensor, &state, "left-up").await;
    wait(vec![("HDMI-1", 0), ("eDP-1", 1)]).await;
    set_orientation(&sensor, &state, "bottom-up").await;
    wait(vec![("HDMI-1", 0), ("eDP-1", 2)]).await;

    // Locked: released, and no more turning.
    gio::Settings::new(helix_comp::rotation::LOCK_SCHEMA).set_boolean("orientation-lock", true).unwrap();
    for _ in 0..100 {
        if state.lock().unwrap().releases > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state.lock().unwrap().releases, 1);
    set_orientation(&sensor, &state, "normal").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(transforms(&services).await, [("HDMI-1".to_owned(), 0), ("eDP-1".to_owned(), 2)]);
    let _ = std::fs::remove_dir_all(&dir);
}
