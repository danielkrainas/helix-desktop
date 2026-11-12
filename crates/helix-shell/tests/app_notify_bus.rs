//! Per-application notification settings (helix-desktop-spec
//! `org.freedesktop.Notifications.md`, "Per-application settings").

use std::collections::HashMap;
use std::time::Duration;

use gio::prelude::*;
use helix_shell::{ShellService, UiEvent};
use helix_test_support::TestBus;
use tokio::sync::mpsc::UnboundedReceiver;
use zbus::zvariant::Value;

const APP: &str = "org.example.Quiet";
const PATH: &str = "/org/gnome/desktop/notifications/application/org-example-quiet/";

/// UI events until things go quiet: (banner summaries, last list's summaries).
async fn drain(ui: &mut UnboundedReceiver<UiEvent>) -> (Vec<String>, Option<Vec<String>>) {
    let (mut banners, mut list) = (Vec::new(), None);
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(300), ui.recv()).await {
        match event {
            UiEvent::Banner(Some(b)) => banners.push(b.summary),
            UiEvent::Notifications(l) => list = Some(l.entries.into_iter().map(|e| e.summary).collect()),
            _ => {}
        }
    }
    (banners, list)
}

async fn notify(conn: &zbus::Connection, summary: &str) -> u32 {
    let hints: HashMap<&str, Value<'_>> = HashMap::from([("desktop-entry", Value::from(format!("{APP}.desktop")))]);
    let reply = conn
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &("Quiet", 0u32, "", summary, "", Vec::<&str>::new(), hints, -1i32),
        )
        .await
        .unwrap();
    reply.body().deserialize().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn per_app_settings() {
    let has_schema = gio::SettingsSchemaSource::default().and_then(|s| s.lookup("org.gnome.desktop.notifications.application", true)).is_some();
    let Some(bus) = TestBus::start() else { return };
    if !has_schema {
        return;
    }
    // SAFETY: this test binary runs this one test.
    unsafe { std::env::set_var("GSETTINGS_BACKEND", "memory") };
    let (service, mut ui) = ShellService::start(bus.builder()).await.unwrap();
    let conn = bus.connect().await;

    // First notification: shown, and the app is registered.
    notify(&conn, "one").await;
    let (banners, _) = drain(&mut ui).await;
    assert_eq!(banners, ["one"]);
    let children: Vec<String> = gio::Settings::new("org.gnome.desktop.notifications").strv("application-children").iter().map(|s| s.to_string()).collect();
    assert!(children.contains(&"org-example-quiet".to_owned()), "{children:?}");
    let schema = gio::SettingsSchemaSource::default().unwrap().lookup("org.gnome.desktop.notifications.application", true).unwrap();
    let app = gio::Settings::new_full(&schema, None::<&gio::SettingsBackend>, Some(PATH));
    assert_eq!(app.string("application-id"), format!("{APP}.desktop"));

    // Banners off: listed, no banner.
    app.set_boolean("show-banners", false).unwrap();
    notify(&conn, "two").await;
    let (banners, list) = drain(&mut ui).await;
    assert!(!banners.contains(&"two".to_owned()), "{banners:?}");
    assert!(list.unwrap_or_default().contains(&"two".to_owned()));

    // Off: dropped, though Notify gives an id.
    app.set_boolean("enable", false).unwrap();
    let id = notify(&conn, "three").await;
    assert!(id > 0);
    let (banners, list) = drain(&mut ui).await;
    assert!(banners.is_empty(), "{banners:?}");
    assert!(list.is_none_or(|l| !l.contains(&"three".to_owned())));
    drop(service);
}
