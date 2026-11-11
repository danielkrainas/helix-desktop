//! The end-of-session sequence over a private bus, with a fake shell dialog
//! and fake session clients.

use std::time::Duration;

use futures_util::StreamExt;
use helix_session::client::SessionManagerProxy;
use helix_session::end_session::Timeouts;
use helix_session::manager::SessionInfo;
use helix_session::{SessionEnd, SessionService};
use helix_test_support::TestBus;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zbus::object_server::SignalEmitter;
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, MatchRule, MessageStream};

const SHORT: Timeouts = Timeouts {
    query: Duration::from_millis(300),
    end: Duration::from_millis(500),
    confirm: Duration::from_secs(10),
};

async fn start(bus: &TestBus) -> JoinHandle<zbus::Result<SessionEnd>> {
    let service = SessionService::start_with_timeouts(bus.builder(), SessionInfo::default(), SHORT)
        .await
        .unwrap();
    tokio::spawn(service.run(std::future::pending()))
}

async fn sm(conn: &Connection) -> SessionManagerProxy<'static> {
    let sm = SessionManagerProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap();
    sm.initialized().await.unwrap();
    sm
}

/// A registered session client that records ClientPrivate signals.
struct TestClient {
    conn: Connection,
    path: OwnedObjectPath,
    signals: MessageStream,
}

impl TestClient {
    async fn register(bus: &TestBus, app_id: &str) -> Self {
        let conn = bus.connect().await;
        let sm = SessionManagerProxy::new(&conn).await.unwrap();
        let path = sm.register_client(app_id, "").await.unwrap();
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.gnome.SessionManager.ClientPrivate")
            .unwrap()
            .path(path.clone())
            .unwrap()
            .build();
        let signals = MessageStream::for_match_rule(rule, &conn, None).await.unwrap();
        Self { conn, path, signals }
    }

    /// Next signal as "Member(flags)" or "Member".
    async fn next(&mut self) -> String {
        let message = tokio::time::timeout(Duration::from_secs(5), self.signals.next())
            .await
            .expect("client signal")
            .unwrap()
            .unwrap();
        let member = message.header().member().unwrap().to_string();
        match message.body().deserialize::<u32>() {
            Ok(flags) => format!("{member}({flags})"),
            Err(_) => member,
        }
    }

    async fn respond(&self, ok: bool, reason: &str) {
        self.conn
            .call_method(
                Some("org.gnome.SessionManager"),
                self.path.as_str(),
                Some("org.gnome.SessionManager.ClientPrivate"),
                "EndSessionResponse",
                &(ok, reason),
            )
            .await
            .unwrap();
    }
}

/// A stand-in for the shell's EndSessionDialog.
struct FakeDialog {
    opens: mpsc::UnboundedSender<(u32, u32, Vec<OwnedObjectPath>)>,
}

#[zbus::interface(name = "org.gnome.SessionManager.EndSessionDialog")]
impl FakeDialog {
    async fn open(&self, kind: u32, _timestamp: u32, seconds: u32, inhibitors: Vec<OwnedObjectPath>) {
        let _ = self.opens.send((kind, seconds, inhibitors));
    }
    async fn close(&self) {}
    #[zbus(signal)]
    async fn confirmed_logout(e: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn confirmed_reboot(e: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn confirmed_shutdown(e: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn canceled(e: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn closed(e: &SignalEmitter<'_>) -> zbus::Result<()>;
}

async fn fake_dialog(bus: &TestBus) -> (Connection, mpsc::UnboundedReceiver<(u32, u32, Vec<OwnedObjectPath>)>) {
    let (opens, rx) = mpsc::unbounded_channel();
    let conn = bus
        .builder()
        .name("org.gnome.Shell")
        .unwrap()
        .serve_at("/org/gnome/SessionManager/EndSessionDialog", FakeDialog { opens })
        .unwrap()
        .build()
        .await
        .unwrap();
    (conn, rx)
}

fn dialog_emitter(conn: &Connection) -> SignalEmitter<'static> {
    SignalEmitter::new(conn, "/org/gnome/SessionManager/EndSessionDialog").unwrap().into_owned()
}

async fn next_open(rx: &mut mpsc::UnboundedReceiver<(u32, u32, Vec<OwnedObjectPath>)>) -> (u32, u32, Vec<OwnedObjectPath>) {
    tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("dialog Open").unwrap()
}

async fn ended(service: JoinHandle<zbus::Result<SessionEnd>>) -> SessionEnd {
    tokio::time::timeout(Duration::from_secs(5), service).await.expect("session ends").unwrap().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn clients_are_queried_ended_and_stopped() {
    let Some(bus) = TestBus::start() else { return };
    let service = start(&bus).await;
    let conn = bus.connect().await;
    let sm = sm(&conn).await;
    let mut client = TestClient::register(&bus, "org.example.Editor").await;

    sm.logout(1).await.unwrap();
    assert_eq!(client.next().await, "QueryEndSession(0)");
    client.respond(true, "").await;
    assert_eq!(client.next().await, "EndSession(0)");
    client.respond(true, "").await;
    assert_eq!(client.next().await, "Stop");
    assert!(matches!(ended(service).await, SessionEnd::Requested(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn refusal_without_a_dialog_cancels() {
    let Some(bus) = TestBus::start() else { return };
    let _service = start(&bus).await;
    let conn = bus.connect().await;
    let sm = sm(&conn).await;
    let mut client = TestClient::register(&bus, "org.example.Editor").await;

    sm.logout(1).await.unwrap();
    assert_eq!(client.next().await, "QueryEndSession(0)");
    client.respond(false, "Unsaved document").await;
    assert_eq!(client.next().await, "CancelEndSession");
    assert!(sm.is_session_running().await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn dialog_cancel_then_confirm() {
    let Some(bus) = TestBus::start() else { return };
    let service = start(&bus).await;
    let (dialog, mut opens) = fake_dialog(&bus).await;
    let conn = bus.connect().await;
    let sm = sm(&conn).await;

    sm.reboot().await.unwrap();
    let (kind, seconds, inhibitors) = next_open(&mut opens).await;
    assert_eq!((kind, seconds, inhibitors.len()), (2, 60, 0));
    FakeDialog::canceled(&dialog_emitter(&dialog)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(sm.is_session_running().await.unwrap(), "cancelled");

    sm.logout(0).await.unwrap();
    assert_eq!(next_open(&mut opens).await.0, 0);
    FakeDialog::confirmed_logout(&dialog_emitter(&dialog)).await.unwrap();
    assert!(matches!(ended(service).await, SessionEnd::Requested(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn dialog_lists_inhibitors_and_refusals() {
    let Some(bus) = TestBus::start() else { return };
    let service = start(&bus).await;
    let (dialog, mut opens) = fake_dialog(&bus).await;
    let conn = bus.connect().await;
    let sm = sm(&conn).await;
    let mut editor = TestClient::register(&bus, "org.example.Editor").await;

    // The editor inhibits logout from its own connection.
    let editor_sm = SessionManagerProxy::new(&editor.conn).await.unwrap();
    editor_sm.inhibit("org.example.Editor", 0, "Copying files", 1).await.unwrap();

    sm.logout(0).await.unwrap();
    let (_, _, inhibitors) = next_open(&mut opens).await;
    assert_eq!(inhibitors.len(), 1);
    let call = |m: &'static str| {
        let conn = conn.clone();
        let path = inhibitors[0].clone();
        async move {
            conn.call_method(Some("org.gnome.SessionManager"), path.as_str(), Some("org.gnome.SessionManager.Inhibitor"), m, &())
                .await
                .unwrap()
        }
    };
    assert_eq!(call("GetAppId").await.body().deserialize::<String>().unwrap(), "org.example.Editor");
    assert_eq!(call("GetReason").await.body().deserialize::<String>().unwrap(), "Copying files");
    assert_eq!(call("GetFlags").await.body().deserialize::<u32>().unwrap(), 1);
    assert_eq!(call("GetClientId").await.body().deserialize::<OwnedObjectPath>().unwrap(), editor.path);

    // "Log out anyway"; then the editor refuses the query.
    FakeDialog::confirmed_logout(&dialog_emitter(&dialog)).await.unwrap();
    assert_eq!(editor.next().await, "QueryEndSession(0)");
    editor.respond(false, "Unsaved document").await;

    // The dialog is asked again, now listing the refusal.
    let (_, _, inhibitors) = next_open(&mut opens).await;
    let mut reasons = Vec::new();
    for p in &inhibitors {
        let reply = conn
            .call_method(Some("org.gnome.SessionManager"), p.as_str(), Some("org.gnome.SessionManager.Inhibitor"), "GetReason", &())
            .await
            .unwrap();
        reasons.push(reply.body().deserialize::<String>().unwrap());
    }
    assert!(reasons.contains(&"Unsaved document".to_owned()), "{reasons:?}");

    FakeDialog::confirmed_logout(&dialog_emitter(&dialog)).await.unwrap();
    assert_eq!(editor.next().await, "EndSession(0)");
    editor.respond(true, "").await;
    assert_eq!(editor.next().await, "Stop");
    assert!(matches!(ended(service).await, SessionEnd::Requested(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn forced_logout_skips_the_query() {
    let Some(bus) = TestBus::start() else { return };
    let service = start(&bus).await;
    let conn = bus.connect().await;
    let sm = sm(&conn).await;
    let mut client = TestClient::register(&bus, "org.example.Editor").await;
    let client_sm = SessionManagerProxy::new(&client.conn).await.unwrap();
    client_sm.inhibit("org.example.Editor", 0, "busy", 1).await.unwrap();

    sm.logout(2).await.unwrap();
    assert_eq!(client.next().await, "EndSession(1)", "forced flag, no query");
    assert_eq!(client.next().await, "Stop");
    assert!(matches!(ended(service).await, SessionEnd::Requested(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn forced_logout_overrides_a_pending_dialog() {
    let Some(bus) = TestBus::start() else { return };
    let service = start(&bus).await;
    let (_dialog, mut opens) = fake_dialog(&bus).await;
    let conn = bus.connect().await;
    let sm = sm(&conn).await;

    sm.logout(0).await.unwrap();
    next_open(&mut opens).await;
    // Nobody answers the dialog; a forced logout must still go through.
    sm.logout(2).await.unwrap();
    assert!(matches!(ended(service).await, SessionEnd::Requested(_)));
}
