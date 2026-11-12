//! Night Light's location from GeoClue (helix-desktop-spec
//! `contracts/gsettings/night-light.md`), against a stand-in GeoClue.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_test_support::TestBus;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::ObjectPath;

type Log = Arc<Mutex<Vec<String>>>;

struct Manager;

#[zbus::interface(name = "org.freedesktop.GeoClue2.Manager")]
impl Manager {
    fn get_client(&self) -> ObjectPath<'static> {
        ObjectPath::from_static_str_unchecked("/org/freedesktop/GeoClue2/Client/1")
    }
}

struct Client {
    log: Log,
}

#[zbus::interface(name = "org.freedesktop.GeoClue2.Client")]
impl Client {
    #[zbus(property)]
    fn desktop_id(&self) -> String {
        String::new()
    }
    #[zbus(property)]
    fn set_desktop_id(&mut self, id: String) {
        self.log.lock().unwrap().push(format!("id {id}"));
    }
    #[zbus(property)]
    fn requested_accuracy_level(&self) -> u32 {
        0
    }
    #[zbus(property)]
    fn set_requested_accuracy_level(&mut self, level: u32) {
        self.log.lock().unwrap().push(format!("accuracy {level}"));
    }
    async fn start(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        self.log.lock().unwrap().push("start".into());
        let emitter = emitter.into_owned();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let old = ObjectPath::from_static_str_unchecked("/");
            let new = ObjectPath::from_static_str_unchecked("/org/freedesktop/GeoClue2/Location/1");
            let _ = Client::location_updated(&emitter, old, new).await;
        });
    }
    fn stop(&self) {
        self.log.lock().unwrap().push("stop".into());
    }
    #[zbus(signal)]
    async fn location_updated(emitter: &SignalEmitter<'_>, old: ObjectPath<'_>, new: ObjectPath<'_>) -> zbus::Result<()>;
}

struct Location;

#[zbus::interface(name = "org.freedesktop.GeoClue2.Location")]
impl Location {
    #[zbus(property)]
    fn latitude(&self) -> f64 {
        52.5
    }
    #[zbus(property)]
    fn longitude(&self) -> f64 {
        13.4
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn locations_come_from_geoclue() {
    let Some(bus) = TestBus::start() else { return };
    let log: Log = Arc::default();
    let _geoclue = bus
        .builder()
        .serve_at("/org/freedesktop/GeoClue2/Manager", Manager)
        .unwrap()
        .serve_at("/org/freedesktop/GeoClue2/Client/1", Client { log: log.clone() })
        .unwrap()
        .serve_at("/org/freedesktop/GeoClue2/Location/1", Location)
        .unwrap()
        .name("org.freedesktop.GeoClue2")
        .unwrap()
        .build()
        .await
        .unwrap();
    let conn = bus.connect().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (done, stop) = tokio::sync::oneshot::channel::<()>();
    let sink = {
        let seen = seen.clone();
        let mut done = Some(done);
        move |lat: f64, lon: f64| {
            seen.lock().unwrap().push((lat, lon));
            if let Some(done) = done.take() {
                let _ = done.send(());
            }
        }
    };
    let tracked = tokio::time::timeout(
        Duration::from_secs(5),
        helix_shell::location::track(&conn, helix_shell::location::accuracy("exact"), sink, async {
            let _ = stop.await;
        }),
    )
    .await;
    assert!(matches!(tracked, Ok(Ok(()))), "{tracked:?}");
    assert_eq!(*seen.lock().unwrap(), [(52.5, 13.4)]);
    assert_eq!(*log.lock().unwrap(), ["id org.gnome.Shell", "accuracy 4", "start", "stop"]);
}
