//! Brightness following a fake ambient light sensor, through a fake
//! DisplayConfig backlight, on a test bus (helix-desktop-spec
//! `contracts/gsettings/power.md`, "Ambient light").

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_test_support::TestBus;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};

struct Sensor {
    level: f64,
    claims: Arc<Mutex<u32>>,
}

#[zbus::interface(name = "net.hadess.SensorProxy")]
impl Sensor {
    #[zbus(property)]
    fn has_ambient_light(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn light_level_unit(&self) -> String {
        "lux".into()
    }
    #[zbus(property)]
    fn light_level(&self) -> f64 {
        self.level
    }
    fn claim_light(&self) {
        *self.claims.lock().unwrap() += 1;
    }
    fn release_light(&self) {}
}

/// A 0–1000 backlight.
struct Display(Arc<Mutex<i32>>);

#[zbus::interface(name = "org.gnome.Mutter.DisplayConfig")]
impl Display {
    #[zbus(property)]
    fn backlight(&self) -> (u32, Vec<HashMap<String, OwnedValue>>) {
        let entry = HashMap::from([
            ("connector".to_owned(), OwnedValue::try_from(Value::from("eDP-1")).unwrap()),
            ("min".to_owned(), OwnedValue::from(0i32)),
            ("max".to_owned(), OwnedValue::from(1000i32)),
            ("value".to_owned(), OwnedValue::from(*self.0.lock().unwrap())),
        ]);
        (1, vec![entry])
    }
    fn set_backlight(&self, _serial: u32, _connector: String, value: i32) {
        *self.0.lock().unwrap() = value;
    }
}

async fn settle(value: &Arc<Mutex<i32>>, want: impl Fn(i32) -> bool) -> i32 {
    for _ in 0..100 {
        let now = *value.lock().unwrap();
        if want(now) {
            return now;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    *value.lock().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn brightness_follows_the_light() {
    let Some(bus) = TestBus::start() else { return };
    let claims = Arc::new(Mutex::new(0));
    let value = Arc::new(Mutex::new(500));
    let sensor = bus.builder().serve_at("/net/hadess/SensorProxy", Sensor { level: 2000.0, claims: claims.clone() }).unwrap().name("net.hadess.SensorProxy").unwrap().build().await.unwrap();
    let _display = bus.builder().serve_at("/org/gnome/Mutter/DisplayConfig", Display(value.clone())).unwrap().name("org.gnome.Mutter.DisplayConfig").unwrap().build().await.unwrap();
    let conn = bus.connect().await;
    let task = tokio::spawn({
        let conn = conn.clone();
        async move { helix_shell::ambient::follow(&conn, &conn).await }
    });

    // Bright light: up to full, in steps.
    let bright = settle(&value, |v| v >= 980).await;
    assert!(bright >= 980, "bright: {bright}");
    assert_eq!(*claims.lock().unwrap(), 1);

    // Dark: down to the floor.
    {
        let iface = sensor.object_server().interface::<_, Sensor>("/net/hadess/SensorProxy").await.unwrap();
        iface.get_mut().await.level = 0.0;
        let emitter = SignalEmitter::new(&sensor, "/net/hadess/SensorProxy").unwrap();
        iface.get().await.light_level_changed(&emitter).await.unwrap();
    }
    let dark = settle(&value, |v| v <= 130).await;
    assert!((90..=130).contains(&dark), "dark: {dark}");

    // The user turns it up: that stays (the curve moves with it).
    tokio::time::sleep(Duration::from_millis(600)).await;
    *value.lock().unwrap() = 600;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let kept = *value.lock().unwrap();
    assert!((580..=620).contains(&kept), "the user's choice stays: {kept}");
    task.abort();
}
