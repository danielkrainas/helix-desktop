//! Notification servers over a private bus. The test plays the shell UI
//! (reading banner events, sending clicks through the UI bridge).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use helix_shell::notifications::Banner;
use helix_shell::{ShellService, UiEvent, UiRequest};
use helix_test_support::TestBus;
use tokio::sync::mpsc::UnboundedReceiver;
use zbus::zvariant::{OwnedValue, Value};

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
    fn close_notification(&self, id: u32) -> zbus::Result<()>;
    fn get_capabilities(&self) -> zbus::Result<Vec<String>>;
    fn get_server_information(&self) -> zbus::Result<(String, String, String, String)>;
    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.gtk.Notifications",
    default_service = "org.gtk.Notifications",
    default_path = "/org/gtk/Notifications"
)]
trait GtkNotifications {
    fn add_notification(&self, app_id: &str, id: &str, notification: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    fn remove_notification(&self, app_id: &str, id: &str) -> zbus::Result<()>;
}

/// Next banner event (skipping unrelated UI events).
async fn next_banner(ui: &mut UnboundedReceiver<UiEvent>) -> Option<Banner> {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), ui.recv())
            .await
            .expect("banner event")
            .expect("ui channel open");
        if let UiEvent::Banner(b) = event {
            return b;
        }
    }
}

async fn next_signal<T>(s: &mut (impl futures_util::Stream<Item = T> + Unpin)) -> T {
    tokio::time::timeout(Duration::from_secs(5), s.next()).await.expect("signal").expect("stream")
}

#[tokio::test(flavor = "multi_thread")]
async fn freedesktop_lifecycle() {
    let Some(bus) = TestBus::start() else { return };
    let (service, mut ui) = ShellService::start(bus.builder()).await.unwrap();
    let conn = bus.connect().await;
    let n = NotificationsProxy::new(&conn).await.unwrap();
    let mut closed = n.receive_notification_closed().await.unwrap();

    assert_eq!(n.get_capabilities().await.unwrap(), ["actions", "body"]);
    let info = n.get_server_information().await.unwrap();
    assert_eq!((info.0.as_str(), info.3.as_str()), ("Helix", "1.2"));

    let id = n.notify("App", 0, "", "Hello", "World", &[], HashMap::new(), -1).await.unwrap();
    assert_ne!(id, 0);
    let banner = next_banner(&mut ui).await.unwrap();
    assert_eq!((banner.id, banner.summary.as_str(), banner.body.as_str()), (id, "Hello", "World"));

    // Replace in place.
    let same = n.notify("App", id, "", "Hello again", "", &[], HashMap::new(), -1).await.unwrap();
    assert_eq!(same, id);
    assert_eq!(next_banner(&mut ui).await.unwrap().summary, "Hello again");

    n.close_notification(id).await.unwrap();
    let args = next_signal(&mut closed).await;
    let args = args.args().unwrap();
    assert_eq!((args.id, args.reason), (id, 3));
    assert_eq!(next_banner(&mut ui).await, None);

    // Expiry.
    let short = n.notify("App", 0, "", "Brief", "", &[], HashMap::new(), 300).await.unwrap();
    let args = next_signal(&mut closed).await;
    let args = args.args().unwrap();
    assert_eq!((args.id, args.reason), (short, 1));
    drop(service);
}

#[tokio::test(flavor = "multi_thread")]
async fn freedesktop_actions() {
    let Some(bus) = TestBus::start() else { return };
    let (service, mut ui) = ShellService::start(bus.builder()).await.unwrap();
    let bridge = service.ui_bridge();
    let conn = bus.connect().await;
    let n = NotificationsProxy::new(&conn).await.unwrap();
    let mut invoked = n.receive_action_invoked().await.unwrap();
    let mut closed = n.receive_notification_closed().await.unwrap();

    let id = n
        .notify("Mail", 0, "", "New mail", "", &["default", "Open", "archive", "Archive"], HashMap::new(), 0)
        .await
        .unwrap();
    let banner = next_banner(&mut ui).await.unwrap();
    assert_eq!(banner.buttons, vec![("archive".to_owned(), "Archive".to_owned())], "default is not a button");

    bridge
        .handle(UiRequest::NotificationActivated { id, action: Some("archive".into()) })
        .await
        .unwrap();
    let a = next_signal(&mut invoked).await;
    let a = a.args().unwrap();
    assert_eq!((a.id, a.action_key.as_str()), (id, "archive"));
    let c = next_signal(&mut closed).await;
    assert_eq!(c.args().unwrap().reason, 2, "dismissed by the user");
}

/// A stand-in GApplication recording ActivateAction / Activate calls.
#[derive(Clone, Default)]
struct FakeApp {
    calls: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.freedesktop.Application")]
impl FakeApp {
    async fn activate(&self, _platform_data: HashMap<String, OwnedValue>) {
        self.calls.lock().unwrap().push("Activate".into());
    }
    async fn open(&self, _uris: Vec<String>, _platform_data: HashMap<String, OwnedValue>) {}
    async fn activate_action(&self, name: String, parameter: Vec<OwnedValue>, _platform_data: HashMap<String, OwnedValue>) {
        let param = parameter
            .first()
            .and_then(|v| <&str>::try_from(&**v).ok().map(str::to_owned))
            .unwrap_or_default();
        self.calls.lock().unwrap().push(format!("ActivateAction {name} {param}"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn gtk_notifications_activate_the_app() {
    let Some(bus) = TestBus::start() else { return };
    let (service, mut ui) = ShellService::start(bus.builder()).await.unwrap();
    let bridge = service.ui_bridge();

    let app = FakeApp::default();
    let _app_conn = bus
        .builder()
        .name("org.example.App")
        .unwrap()
        .serve_at("/org/example/App", app.clone())
        .unwrap()
        .build()
        .await
        .unwrap();

    let conn = bus.connect().await;
    let gtk = GtkNotificationsProxy::new(&conn).await.unwrap();
    let notification = HashMap::from([
        ("title", Value::from("Download complete")),
        ("body", Value::from("file.iso")),
        ("default-action", Value::from("app.open")),
        // GLib wraps the target in its own variant inside the a{sv} value.
        ("default-action-target", Value::Value(Box::new(Value::from("file.iso")))),
    ]);
    gtk.add_notification("org.example.App", "dl-1", notification).await.unwrap();
    let banner = next_banner(&mut ui).await.unwrap();
    assert_eq!(banner.summary, "Download complete");

    bridge.handle(UiRequest::NotificationActivated { id: banner.id, action: None }).await.unwrap();
    assert_eq!(app.calls.lock().unwrap().as_slice(), ["ActivateAction open file.iso"]);
    assert_eq!(next_banner(&mut ui).await, None, "removed after activation");

    // A non-`app.` action: no activation, an ActionInvoked signal instead.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.gtk.Notifications")
        .unwrap()
        .member("ActionInvoked")
        .unwrap()
        .build();
    let mut invoked = zbus::MessageStream::for_match_rule(rule, &conn, None).await.unwrap();
    let notification = HashMap::from([
        ("title", Value::from("Reply?")),
        ("default-action", Value::from("win.reply")),
        ("default-action-target", Value::Value(Box::new(Value::from(7u32)))),
    ]);
    gtk.add_notification("org.example.App", "msg-1", notification).await.unwrap();
    let banner = next_banner(&mut ui).await.unwrap();
    bridge.handle(UiRequest::NotificationActivated { id: banner.id, action: None }).await.unwrap();
    let message = tokio::time::timeout(std::time::Duration::from_secs(5), futures_util::StreamExt::next(&mut invoked))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let (app_id, id, action, parameter, _): (String, String, String, Vec<zbus::zvariant::OwnedValue>, HashMap<String, zbus::zvariant::OwnedValue>) =
        message.body().deserialize().unwrap();
    assert_eq!((app_id.as_str(), id.as_str(), action.as_str()), ("org.example.App", "msg-1", "win.reply"));
    assert_eq!(parameter.len(), 1);
    assert_eq!(app.calls.lock().unwrap().len(), 1, "not activated");
    assert_eq!(next_banner(&mut ui).await, None);

    // Add then withdraw.
    gtk.add_notification("org.example.App", "dl-2", HashMap::from([("title", Value::from("Second"))]))
        .await
        .unwrap();
    assert_eq!(next_banner(&mut ui).await.unwrap().summary, "Second");
    gtk.remove_notification("org.example.App", "dl-2").await.unwrap();
    assert_eq!(next_banner(&mut ui).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn banners_wait_while_locked() {
    let Some(bus) = TestBus::start() else { return };
    let (service, mut ui) = ShellService::start(bus.builder()).await.unwrap();
    let bridge = service.ui_bridge();
    let conn = bus.connect().await;
    let n = NotificationsProxy::new(&conn).await.unwrap();

    bridge.handle(UiRequest::Locked).await.unwrap();
    n.notify("App", 0, "", "While locked", "", &[], HashMap::new(), 0).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(event) = ui.try_recv() {
        assert!(!matches!(event, UiEvent::Banner(Some(_))), "nothing shown while locked");
    }
    bridge.handle(UiRequest::Unlocked).await.unwrap();
    assert_eq!(next_banner(&mut ui).await.unwrap().summary, "While locked");
}
