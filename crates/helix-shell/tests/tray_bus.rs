//! Status icons registered with the shell's StatusNotifierWatcher on a
//! test bus (helix-desktop-spec
//! `contracts/dbus/org.kde.StatusNotifierItem.md`).

use std::time::Duration;

use helix_shell::UiEvent;
use helix_test_support::TestBus;

struct Item;

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    #[zbus(property)]
    fn status(&self) -> String {
        "Active".into()
    }
    #[zbus(property)]
    fn icon_name(&self) -> String {
        "mail-unread".into()
    }
    #[zbus(property)]
    fn title(&self) -> String {
        "Test".into()
    }
}

async fn last_tray(rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiEvent>) -> Option<Vec<helix_shell::tray::Item>> {
    let mut last = None;
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
        if let UiEvent::Tray(items) = event {
            last = Some(items);
        }
    }
    last
}

#[tokio::test(flavor = "multi_thread")]
async fn items_register_and_go_away() {
    let Some(bus) = TestBus::start() else { return };
    let (ui, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tray = helix_shell::tray::Tray::new(ui);
    let watcher = bus.builder().serve_at(helix_shell::tray::OBJECT_PATH, helix_shell::tray::Watcher { tray }).unwrap().name(helix_shell::tray::BUS_NAME).unwrap().build().await.unwrap();

    // libayatana-appindicator style: an object path on the caller.
    let app = bus.builder().serve_at("/org/ayatana/NotificationItem/test", Item).unwrap().build().await.unwrap();
    let watcher_proxy = zbus::Proxy::new(&app, helix_shell::tray::BUS_NAME, helix_shell::tray::OBJECT_PATH, "org.kde.StatusNotifierWatcher").await.unwrap();
    watcher_proxy.call_method("RegisterStatusNotifierItem", &("/org/ayatana/NotificationItem/test",)).await.unwrap();
    // Qt style: a bus name, the item at /StatusNotifierItem.
    let qt = bus.builder().serve_at("/StatusNotifierItem", Item).unwrap().name("org.kde.StatusNotifierItem-1-1").unwrap().build().await.unwrap();
    let qt_proxy = zbus::Proxy::new(&qt, helix_shell::tray::BUS_NAME, helix_shell::tray::OBJECT_PATH, "org.kde.StatusNotifierWatcher").await.unwrap();
    qt_proxy.call_method("RegisterStatusNotifierItem", &("org.kde.StatusNotifierItem-1-1",)).await.unwrap();
    // A name nobody owns is refused.
    assert!(qt_proxy.call_method("RegisterStatusNotifierItem", &("org.kde.StatusNotifierItem-9-9",)).await.is_err());

    let items = last_tray(&mut rx).await.expect("items published");
    let app_key = format!("{}/org/ayatana/NotificationItem/test", app.unique_name().unwrap());
    let qt_key = format!("{}/StatusNotifierItem", qt.unique_name().unwrap());
    assert_eq!(items.iter().map(|i| i.key.clone()).collect::<Vec<_>>(), [app_key.clone(), qt_key.clone()]);
    assert!(items.iter().all(|i| i.shown() && i.icon.name == "mail-unread" && i.title == "Test"));
    let listed: Vec<String> = watcher_proxy.get_property("RegisteredStatusNotifierItems").await.unwrap();
    assert_eq!(listed, [app_key.clone(), qt_key]);

    // An app leaving the bus takes its item with it.
    drop(qt_proxy);
    qt.close().await.unwrap();
    let items = last_tray(&mut rx).await.expect("removal published");
    assert_eq!(items.iter().map(|i| i.key.clone()).collect::<Vec<_>>(), [app_key]);
    drop(watcher);
}
