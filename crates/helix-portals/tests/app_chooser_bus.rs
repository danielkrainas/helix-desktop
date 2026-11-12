//! The AppChooser backend's `UpdateChoices` against a fake shell, on a test
//! bus (helix-desktop-spec `org.freedesktop.impl.portal.md`, "AppChooser").

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gio::prelude::*;
use helix_portals::settings::Snapshot;
use helix_portals::{PortalService, BUS_NAME, OBJECT_PATH};
use helix_session::SessionService;
use helix_test_support::TestBus;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

type Items = Vec<(String, String)>;

#[derive(Default)]
struct Shared {
    shown: Mutex<Option<Items>>,
    updated: Mutex<Option<Items>>,
    notify: tokio::sync::Notify,
}

struct Shell(Arc<Shared>);

#[zbus::interface(name = "org.helix.PortalDialog")]
impl Shell {
    async fn choose_app(&self, _title: String, _message: String, items: Items, _selected: String) -> String {
        *self.0.shown.lock().unwrap() = Some(items);
        // Answer once the list was updated: the first new item.
        self.0.notify.notified().await;
        self.0.updated.lock().unwrap().as_ref().and_then(|i| i.first()).map(|(id, _)| id.clone()).unwrap_or_default()
    }
    async fn update_apps(&self, items: Items) -> bool {
        *self.0.updated.lock().unwrap() = Some(items);
        self.0.notify.notify_one();
        true
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn choices_change_while_open() {
    let Some(bus) = TestBus::start() else { return };
    // Two apps this machine has.
    let apps: Vec<String> = gio::AppInfo::all().into_iter().filter(|a| a.should_show()).filter_map(|a| a.id().map(|i| i.trim_end_matches(".desktop").to_owned())).take(2).collect();
    if apps.len() < 2 {
        return;
    }
    let shared = Arc::new(Shared::default());
    let _shell = bus.builder().serve_at("/org/helix/PortalDialog", Shell(shared.clone())).unwrap().name("org.gnome.Shell").unwrap().build().await.unwrap();
    let session = SessionService::start(bus.builder()).await.unwrap();
    let _session = tokio::spawn(session.run(std::future::pending()));
    let settings: Arc<Mutex<Snapshot>> = Arc::new(Mutex::new(BTreeMap::new()));
    let _portals = PortalService::start(bus.builder(), settings).await.unwrap();
    let app = bus.connect().await;
    const IFACE: &str = "org.freedesktop.impl.portal.AppChooser";
    let handle = OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/request/1_1/open1").unwrap();

    // Not open yet.
    assert!(app.call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "UpdateChoices", &(handle.clone(), vec![apps[1].as_str()])).await.is_err());

    let call = tokio::spawn({
        let (app, handle, first) = (app.clone(), handle.clone(), apps[0].clone());
        async move {
            let reply = app
                .call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "ChooseApplication", &(handle, "org.example.Viewer", "", vec![first.as_str()], HashMap::<String, OwnedValue>::new()))
                .await
                .unwrap();
            reply.body().deserialize::<(u32, HashMap<String, OwnedValue>)>().unwrap()
        }
    });
    for _ in 0..100 {
        if shared.shown.lock().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(shared.shown.lock().unwrap().as_ref().map(|i| i.len()), Some(1));
    app.call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "UpdateChoices", &(handle.clone(), vec![apps[1].as_str(), apps[0].as_str(), "not-an-app"]))
        .await
        .unwrap();
    let (code, results) = tokio::time::timeout(Duration::from_secs(5), call).await.unwrap().unwrap();
    assert_eq!(code, 0);
    assert_eq!(String::try_from(results["choice"].clone()).unwrap(), apps[1], "chosen from the new list");
    let updated: Vec<String> = shared.updated.lock().unwrap().clone().unwrap().into_iter().map(|(id, _)| id).collect();
    assert_eq!(updated, [apps[1].clone(), apps[0].clone()], "uninstalled apps left out");
}
