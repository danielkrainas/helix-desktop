//! The Screenshot portal backend, against a stand-in
//! `org.gnome.Shell.Screenshot` (its own test binary: it points HOME at a
//! scratch directory before GLib reads it).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use helix_portals::settings::Snapshot;
use helix_portals::PortalService;
use helix_session::SessionService;
use helix_test_support::TestBus;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::{fdo, interface};

#[zbus::proxy(
    interface = "org.freedesktop.impl.portal.Screenshot",
    default_service = "org.freedesktop.impl.portal.desktop.helix",
    default_path = "/org/freedesktop/portal/desktop"
)]
trait Screenshot {
    fn screenshot(
        &self,
        handle: &ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<(u32, HashMap<String, OwnedValue>)>;
    fn pick_color(
        &self,
        handle: &ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<(u32, HashMap<String, OwnedValue>)>;
    #[zbus(property)]
    fn available_targets(&self) -> zbus::Result<u32>;
    #[zbus(property, name = "version")]
    fn version(&self) -> zbus::Result<u32>;
}

/// Records calls; answers as a cooperative shell would.
struct FakeShell {
    calls: Arc<Mutex<Vec<String>>>,
    interactive: Arc<Mutex<Vec<(bool, String)>>>,
}

#[interface(name = "org.gnome.Shell.Screenshot")]
impl FakeShell {
    async fn interactive_screenshot(&self) -> (bool, String) {
        self.calls.lock().unwrap().push("InteractiveScreenshot".into());
        self.interactive.lock().unwrap().remove(0)
    }

    async fn screenshot(&self, include_cursor: bool, flash: bool, filename: &str) -> (bool, String) {
        self.calls.lock().unwrap().push(format!("Screenshot {include_cursor} {flash}"));
        (true, filename.to_owned())
    }

    async fn screenshot_window(&self, _frame: bool, _cursor: bool, _flash: bool, filename: &str) -> (bool, String) {
        self.calls.lock().unwrap().push("ScreenshotWindow".into());
        (false, filename.to_owned())
    }

    async fn select_area(&self) -> fdo::Result<(i32, i32, i32, i32)> {
        self.calls.lock().unwrap().push("SelectArea".into());
        Err(fdo::Error::Failed("Operation was cancelled".into()))
    }

    async fn pick_color(&self) -> HashMap<String, OwnedValue> {
        self.calls.lock().unwrap().push("PickColor".into());
        HashMap::from([("color".to_owned(), OwnedValue::try_from(Value::from((0.5, 0.25, 1.0))).unwrap())])
    }
}

/// The shell's window pick: cancelled the first time, then a window.
struct FakePicker {
    calls: Arc<Mutex<Vec<String>>>,
}

#[interface(name = "org.helix.ScreenshotWindowPicker")]
impl FakePicker {
    async fn pick_window(&self, _cursor: bool, _flash: bool, filename: &str) -> fdo::Result<(bool, String)> {
        let mut calls = self.calls.lock().unwrap();
        calls.push("PickWindow".into());
        if calls.iter().filter(|c| *c == "PickWindow").count() == 1 {
            return Err(fdo::Error::Failed("Operation was cancelled".into()));
        }
        Ok((true, filename.to_owned()))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn screenshots_go_through_the_shell() {
    let home = std::env::temp_dir().join(format!("helix-portal-shot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    // Before any other thread or GLib reads the environment.
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", home.join("config"));

    let Some(bus) = TestBus::start() else { return };
    let session = SessionService::start(bus.builder()).await.unwrap();
    let _session = tokio::spawn(session.run(std::future::pending()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let interactive = Arc::new(Mutex::new(vec![(true, "file:///picked.png".to_owned()), (false, String::new())]));
    let _shell = bus
        .builder()
        .serve_at("/org/gnome/Shell/Screenshot", FakeShell { calls: calls.clone(), interactive })
        .unwrap()
        .serve_at("/org/gnome/Shell/Screenshot", FakePicker { calls: calls.clone() })
        .unwrap()
        .name("org.gnome.Shell")
        .unwrap()
        .build()
        .await
        .unwrap();
    let settings: Arc<Mutex<Snapshot>> = Arc::new(Mutex::new(BTreeMap::new()));
    let _portals = PortalService::start(bus.builder(), settings).await.unwrap();

    let conn = bus.connect().await;
    let portal = ScreenshotProxy::builder(&conn).cache_properties(CacheProperties::No).build().await.unwrap();
    assert_eq!(portal.available_targets().await.unwrap(), 15);
    assert_eq!(portal.version().await.unwrap(), 3);
    let handle = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1_1/t1").unwrap();
    let checked = || HashMap::from([("permission_store_checked", Value::from(true))]);
    let uri = |results: &HashMap<String, OwnedValue>| String::try_from(results["uri"].clone()).unwrap();

    // Not checked yet: the user decides.
    let (response, results) = portal.screenshot(&handle, "org.example.App", "", HashMap::new()).await.unwrap();
    assert_eq!((response, uri(&results).as_str()), (0, "file:///picked.png"));
    let (response, _) = portal.screenshot(&handle, "org.example.App", "", HashMap::new()).await.unwrap();
    assert_eq!(response, 1, "cancelled in the screenshot UI");

    // Checked: the screen at once, saved in Screenshots.
    let (response, results) = portal.screenshot(&handle, "org.example.App", "", checked()).await.unwrap();
    assert_eq!(response, 0);
    let saved = uri(&results);
    let screenshots = gio_uri(&home.join("Screenshots"));
    assert!(saved.starts_with(&format!("{screenshots}/Screenshot%20From%20")), "{saved} in {screenshots}");

    let mut area = checked();
    area.insert("target", Value::from(4u32));
    assert_eq!(portal.screenshot(&handle, "a", "", area).await.unwrap().0, 1, "selection cancelled");
    let mut window = checked();
    window.insert("target", Value::from(8u32));
    assert_eq!(portal.screenshot(&handle, "a", "", window).await.unwrap().0, 2, "shell failed");
    let mut picked = checked();
    picked.insert("target", Value::from(2u32));
    assert_eq!(portal.screenshot(&handle, "a", "", picked.clone()).await.unwrap().0, 1, "window pick cancelled");
    let (response, results) = portal.screenshot(&handle, "a", "", picked).await.unwrap();
    assert_eq!(response, 0, "a window picked");
    assert!(uri(&results).contains("Screenshot%20From%20"));
    let mut other = checked();
    other.insert("target", Value::from(16u32));
    assert_eq!(portal.screenshot(&handle, "a", "", other).await.unwrap().0, 2, "not offered");

    let (response, results) = portal.pick_color(&handle, "a", "", HashMap::new()).await.unwrap();
    assert_eq!(response, 0);
    assert_eq!(<(f64, f64, f64)>::try_from(results["color"].clone()).unwrap(), (0.5, 0.25, 1.0));

    assert_eq!(
        *calls.lock().unwrap(),
        ["InteractiveScreenshot", "InteractiveScreenshot", "Screenshot false false", "SelectArea", "ScreenshotWindow", "PickWindow", "PickWindow", "PickColor"]
    );
    // The Request objects are gone afterwards.
    let close = conn
        .call_method(Some(helix_portals::BUS_NAME), handle.as_str(), Some("org.freedesktop.impl.portal.Request"), "Close", &())
        .await;
    assert!(close.is_err(), "no Request object left");
    std::fs::remove_dir_all(&home).unwrap();
}

fn gio_uri(path: &std::path::Path) -> String {
    format!("file://{}", path.display())
}
