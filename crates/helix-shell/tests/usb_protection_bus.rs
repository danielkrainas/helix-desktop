//! USB protection against a fake USBGuard on a test bus (helix-desktop-spec
//! `org.gnome.desktop.privacy.md`, "USB protection").

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gio::prelude::*;
use helix_test_support::TestBus;
use zbus::object_server::SignalEmitter;

struct Devices(Arc<Mutex<Vec<(u32, u32)>>>);

#[zbus::interface(name = "org.usbguard.Devices1")]
impl Devices {
    #[zbus(name = "applyDevicePolicy")]
    fn apply_device_policy(&self, id: u32, target: u32, _permanent: bool) -> u32 {
        self.0.lock().unwrap().push((id, target));
        0
    }
    #[zbus(signal, name = "DevicePresenceChanged")]
    async fn device_presence_changed(emitter: &SignalEmitter<'_>, id: u32, event: u32, target: u32, device_rule: &str, attributes: HashMap<String, String>) -> zbus::Result<()>;
}

#[tokio::test(flavor = "multi_thread")]
async fn devices_plugged_in_are_rejected() {
    if !helix_settings::has_schema("org.gnome.desktop.privacy") || !gio::SettingsSchemaSource::default().and_then(|s| s.lookup("org.gnome.desktop.privacy", true)).is_some_and(|s| s.has_key("usb-protection")) {
        return;
    }
    let Some(bus) = TestBus::start() else { return };
    // SAFETY: this test binary runs this one test.
    unsafe { std::env::set_var("GSETTINGS_BACKEND", "memory") };
    let privacy = gio::Settings::new("org.gnome.desktop.privacy");
    privacy.set_boolean("usb-protection", true).unwrap();
    privacy.set_string("usb-protection-level", "always").unwrap();

    let applied = Arc::new(Mutex::new(Vec::new()));
    let guard = bus.builder().serve_at("/org/usbguard1/Devices", Devices(applied.clone())).unwrap().name("org.usbguard1").unwrap().build().await.unwrap();
    let conn = bus.connect().await;
    let (ui, _rx) = tokio::sync::mpsc::unbounded_channel();
    let center = helix_shell::notifications::Center::new(ui, Default::default());
    tokio::spawn(async move { helix_shell::usb_protection::follow(&conn, &center).await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let emitter = SignalEmitter::new(&guard, "/org/usbguard1/Devices").unwrap();
    let stick = r#"allow id 0781:5567 name "Cruzer" with-interface 08:06:50"#;
    let keyboard = r#"allow id 046d:c31c name "Keyboard" with-interface { 03:01:01 03:00:00 }"#;
    Devices::device_presence_changed(&emitter, 7, 1, 0, stick, HashMap::new()).await.unwrap();
    Devices::device_presence_changed(&emitter, 8, 1, 0, keyboard, HashMap::new()).await.unwrap();
    // Already there (event 0): left alone.
    Devices::device_presence_changed(&emitter, 9, 0, 0, stick, HashMap::new()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(*applied.lock().unwrap(), [(7, helix_shell::usb_protection::REJECT)]);
}
