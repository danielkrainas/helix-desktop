//! Portal backends end to end on a private bus, with a real SessionManager.
//! The test plays the role of the xdg-desktop-portal frontend.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use helix_portals::settings::{Snapshot, APPEARANCE};
use helix_portals::{PortalService, BUS_NAME, OBJECT_PATH};
use helix_session::client::SessionManagerProxy;
use helix_session::SessionService;
use helix_test_support::TestBus;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{ObjectPath, OwnedValue, Str, Value};
use zbus::Connection;

#[zbus::proxy(
    interface = "org.freedesktop.impl.portal.Settings",
    default_service = "org.freedesktop.impl.portal.desktop.helix",
    default_path = "/org/freedesktop/portal/desktop"
)]
trait Settings {
    fn read_all(&self, namespaces: &[&str]) -> zbus::Result<HashMap<String, HashMap<String, OwnedValue>>>;
    fn read(&self, namespace: &str, key: &str) -> zbus::Result<OwnedValue>;
    #[zbus(signal)]
    fn setting_changed(&self, namespace: String, key: String, value: OwnedValue) -> zbus::Result<()>;
    #[zbus(property, name = "version")]
    fn version(&self) -> zbus::Result<u32>;
}

#[zbus::proxy(
    interface = "org.freedesktop.impl.portal.Inhibit",
    default_service = "org.freedesktop.impl.portal.desktop.helix",
    default_path = "/org/freedesktop/portal/desktop"
)]
trait Inhibit {
    fn inhibit(
        &self,
        handle: &ObjectPath<'_>,
        app_id: &str,
        window: &str,
        flags: u32,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;
    fn create_monitor(
        &self,
        handle: &ObjectPath<'_>,
        session_handle: &ObjectPath<'_>,
        app_id: &str,
        window: &str,
    ) -> zbus::Result<u32>;
    fn query_end_response(&self, session_handle: &ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    fn state_changed(&self, session_handle: ObjectPath<'_>, state: HashMap<String, OwnedValue>) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.impl.portal.Request",
    default_service = "org.freedesktop.impl.portal.desktop.helix"
)]
trait Request {
    fn close(&self) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.impl.portal.Session",
    default_service = "org.freedesktop.impl.portal.desktop.helix"
)]
trait PortalSession {
    fn close(&self) -> zbus::Result<()>;
}

fn source(scheme: &str) -> Snapshot {
    BTreeMap::from([(
        "org.gnome.desktop.interface".to_owned(),
        BTreeMap::from([
            ("color-scheme".to_owned(), OwnedValue::from(Str::from(scheme.to_owned()))),
            ("enable-animations".to_owned(), OwnedValue::from(true)),
        ]),
    )])
}

struct Fixture {
    bus: TestBus,
    _session: tokio::task::JoinHandle<zbus::Result<helix_session::SessionEnd>>,
    portals: PortalService,
    settings: Arc<Mutex<Snapshot>>,
}

async fn fixture() -> Option<Fixture> {
    let bus = TestBus::start()?;
    let session = SessionService::start(bus.builder()).await.unwrap();
    let session = tokio::spawn(session.run(std::future::pending()));
    let settings = Arc::new(Mutex::new(source("default")));
    let portals = PortalService::start(bus.builder(), settings.clone()).await.unwrap();
    Some(Fixture {
        bus,
        _session: session,
        portals,
        settings,
    })
}

async fn sm(conn: &Connection) -> SessionManagerProxy<'static> {
    SessionManagerProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap()
}

async fn wait_until<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_read_and_change() {
    let Some(f) = fixture().await else { return };
    let conn = f.bus.connect().await;
    let settings = SettingsProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap();
    assert_eq!(settings.version().await.unwrap(), 1);

    let all = settings.read_all(&[]).await.unwrap();
    assert!(all.contains_key(APPEARANCE));
    assert!(all.contains_key("org.gnome.desktop.interface"));
    let only = settings.read_all(&["org.freedesktop.*"]).await.unwrap();
    assert_eq!(only.keys().collect::<Vec<_>>(), vec![APPEARANCE]);

    let v = settings.read(APPEARANCE, "color-scheme").await.unwrap();
    assert_eq!(u32::try_from(&*v).unwrap(), 0);

    let err = settings.read(APPEARANCE, "no-such-key").await.unwrap_err();
    match err {
        zbus::Error::MethodError(name, _, _) => {
            assert_eq!(name.as_str(), "org.freedesktop.portal.Error.NotFound")
        }
        other => panic!("unexpected error {other:?}"),
    }

    let mut changed = settings.receive_setting_changed().await.unwrap();
    *f.settings.lock().unwrap() = source("prefer-dark");
    f.portals.settings_changed().await.unwrap();

    let mut seen = Vec::new();
    for _ in 0..2 {
        let sig = tokio::time::timeout(Duration::from_secs(5), changed.next())
            .await
            .expect("SettingChanged")
            .unwrap();
        let args = sig.args().unwrap();
        seen.push(format!("{} {}", args.namespace, args.key));
        if args.namespace == APPEARANCE {
            assert_eq!(u32::try_from(&*args.value).unwrap(), 1);
        }
    }
    seen.sort();
    assert_eq!(
        seen,
        vec![
            "org.freedesktop.appearance color-scheme",
            "org.gnome.desktop.interface color-scheme"
        ]
    );
    let v = settings.read(APPEARANCE, "color-scheme").await.unwrap();
    assert_eq!(u32::try_from(&*v).unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn inhibit_maps_to_session_manager_until_request_closed() {
    let Some(f) = fixture().await else { return };
    let frontend = f.bus.connect().await;
    let inhibit = InhibitProxy::new(&frontend).await.unwrap();
    let sm = sm(&frontend).await;
    sm.initialized().await.unwrap();

    let handle = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1_1/t1").unwrap();
    inhibit
        .inhibit(&handle, "org.example.App", "", 1 | 8, HashMap::from([("reason", Value::from("rendering"))]))
        .await
        .unwrap();
    assert!(sm.is_inhibited(1).await.unwrap());
    assert!(sm.is_inhibited(8).await.unwrap());
    assert_eq!(sm.inhibited_actions().await.unwrap(), 9);
    // The logout flag blocks logout through the portal too.
    assert!(sm.logout(0).await.is_err());

    let request = RequestProxy::builder(&frontend).path(&handle).unwrap().build().await.unwrap();
    request.close().await.unwrap();
    assert!(!sm.is_inhibited(u32::MAX).await.unwrap());
    assert_eq!(f.portals.registry().lock().unwrap().request_count(), 0);
    // The Request object is gone.
    assert!(request.close().await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn frontend_disconnect_releases_inhibitors() {
    let Some(f) = fixture().await else { return };
    let observer = f.bus.connect().await;
    let sm = sm(&observer).await;
    {
        let frontend = f.bus.connect().await;
        let inhibit = InhibitProxy::new(&frontend).await.unwrap();
        let handle = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1_2/t1").unwrap();
        inhibit.inhibit(&handle, "org.example.App", "", 4, HashMap::new()).await.unwrap();
        assert!(sm.is_inhibited(4).await.unwrap());
    }
    wait_until("inhibitor release", || async { !sm.is_inhibited(u32::MAX).await.unwrap() }).await;
    assert_eq!(f.portals.registry().lock().unwrap().request_count(), 0);
}

async fn next_state(states: &mut StateChangedStream, session: &ObjectPath<'_>) -> u32 {
    let sig = tokio::time::timeout(Duration::from_secs(5), states.next())
        .await
        .expect("StateChanged")
        .unwrap();
    let args = sig.args().unwrap();
    assert_eq!(args.session_handle.as_str(), session.as_str());
    assert!(!bool::try_from(&*args.state["screensaver-active"]).unwrap());
    u32::try_from(&*args.state["session-state"]).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn monitor_reports_running_then_ending() {
    let Some(f) = fixture().await else { return };
    let frontend = f.bus.connect().await;
    let inhibit = InhibitProxy::new(&frontend).await.unwrap();
    let sm = sm(&frontend).await;
    sm.initialized().await.unwrap();
    let mut states = inhibit.receive_state_changed().await.unwrap();

    let handle = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1_3/t1").unwrap();
    let session = ObjectPath::try_from("/org/freedesktop/portal/desktop/session/1_3/s1").unwrap();
    assert_eq!(inhibit.create_monitor(&handle, &session, "org.example.App", "").await.unwrap(), 0);
    inhibit.query_end_response(&session).await.unwrap();

    assert_eq!(next_state(&mut states, &session).await, 1);

    sm.logout(1).await.unwrap();
    // Query end first; a monitor acknowledges, then the session ends.
    assert_eq!(next_state(&mut states, &session).await, 2);
    inhibit.query_end_response(&session).await.unwrap();
    assert_eq!(next_state(&mut states, &session).await, 3);

    let s = PortalSessionProxy::builder(&frontend).path(&session).unwrap().build().await.unwrap();
    s.close().await.unwrap();
    assert_eq!(f.portals.registry().lock().unwrap().monitor_count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn owns_backend_name() {
    let Some(f) = fixture().await else { return };
    let conn = f.bus.connect().await;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
    assert!(dbus.name_has_owner(BUS_NAME.try_into().unwrap()).await.unwrap());
    // Shut down with a live request and monitor still exported.
    let inhibit = InhibitProxy::new(&conn).await.unwrap();
    let handle = ObjectPath::try_from(format!("{OBJECT_PATH}/request/1_4/t1")).unwrap();
    let session = ObjectPath::try_from(format!("{OBJECT_PATH}/session/1_4/s1")).unwrap();
    inhibit.inhibit(&handle, "org.example.App", "", 1, HashMap::new()).await.unwrap();
    inhibit.create_monitor(&handle, &session, "org.example.App", "").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), f.portals.shutdown())
        .await
        .expect("shutdown completes")
        .unwrap();
    assert!(!dbus.name_has_owner(BUS_NAME.try_into().unwrap()).await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn bus_going_away_lets_the_backend_exit() {
    let Some(f) = fixture().await else { return };
    let Fixture { bus, mut portals, .. } = f;
    drop(bus);
    tokio::time::timeout(Duration::from_secs(5), portals.bus_closed())
        .await
        .expect("the backend notices the bus is gone");
    tokio::time::timeout(Duration::from_secs(5), portals.shutdown())
        .await
        .expect("shutdown does not wait for a bus that is gone")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_is_bounded() {
    let Some(f) = fixture().await else { return };
    // A clone held elsewhere (as by a call in progress) would keep a
    // graceful shutdown waiting forever.
    let _held = f.portals.connection().clone();
    tokio::time::timeout(helix_portals::SHUTDOWN_TIMEOUT + Duration::from_secs(3), f.portals.shutdown())
        .await
        .expect("shutdown gives up waiting")
        .unwrap();
}
