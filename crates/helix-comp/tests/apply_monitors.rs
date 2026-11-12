//! `ApplyMonitorsConfig` over a private bus, with a stand-in compositor
//! that applies what it is asked to.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use helix_comp::display_config::{ApplyRequest, Layout, Mode, Monitor, Store, ALL_TRANSFORMS};
use helix_comp::{CompositorServices, Event, MonitorControl};
use helix_test_support::TestBus;
use tokio::sync::mpsc;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedValue, Value};

type Props = HashMap<String, OwnedValue>;
type WireMonitor = ((String, String, String, String), Vec<(String, i32, i32, f64, f64, Vec<f64>, Props)>, Props);
type WireLogical = (i32, i32, f64, u32, bool, Vec<(String, String, String, String)>, Props);
type ApplyLogical<'a> = (i32, i32, f64, u32, bool, Vec<(&'a str, &'a str, HashMap<&'a str, Value<'a>>)>);

#[zbus::proxy(
    interface = "org.gnome.Mutter.DisplayConfig",
    default_service = "org.gnome.Mutter.DisplayConfig",
    default_path = "/org/gnome/Mutter/DisplayConfig"
)]
trait DisplayConfig {
    fn get_current_state(&self) -> zbus::Result<(u32, Vec<WireMonitor>, Vec<WireLogical>, Props)>;
    fn apply_monitors_config(
        &self,
        serial: u32,
        method: u32,
        logical_monitors: &[ApplyLogical<'_>],
        properties: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    fn monitors_changed(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn apply_monitors_config_allowed(&self) -> zbus::Result<bool>;
}

const FHD: &str = "1920x1080@60.000";

fn monitor(connector: &str, x: i32, primary: bool) -> Monitor {
    Monitor {
        connector: connector.into(),
        vendor: "Acme".into(),
        product: "Panel".into(),
        serial: connector.into(),
        display_name: connector.into(),
        builtin: false,
        modes: vec![Mode { width: 1920, height: 1080, refresh_mhz: 60_000, preferred: true, current: true }],
        x,
        y: 0,
        scale: 1.0,
        transform: 0,
        primary,
        transforms: ALL_TRANSFORMS.to_vec(),
        hdr: false,
        color_mode: 0,
        underscan: None,
    }
}

fn both_on() -> Vec<Monitor> {
    vec![monitor("DP-1", 0, true), monitor("HDMI-1", 1920, false)]
}

/// Applies requests to its copy of the monitors; counts them.
fn fake_compositor(initial: Vec<Monitor>) -> (mpsc::UnboundedSender<ApplyRequest>, Arc<Mutex<usize>>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<ApplyRequest>();
    let count = Arc::new(Mutex::new(0));
    let counted = count.clone();
    tokio::spawn(async move {
        let mut current = initial;
        while let Some(request) = rx.recv().await {
            *counted.lock().unwrap() += 1;
            current = Layout::new(current).applied(&request.config);
            let _ = request.reply.send(Ok(current.clone()));
        }
    });
    (tx, count)
}

struct Running {
    events: mpsc::UnboundedSender<Event>,
    applied: Arc<Mutex<usize>>,
}

async fn start(bus: &TestBus, store: &Path, monitors: Vec<Monitor>) -> Running {
    start_confirming(bus, store, monitors, 20).await
}

async fn start_confirming(bus: &TestBus, store: &Path, monitors: Vec<Monitor>, confirm_seconds: u32) -> Running {
    let (applier, applied) = fake_compositor(monitors.clone());
    let control = MonitorControl { applier: Some(applier), store: Store::at(store.to_path_buf()), confirm_seconds, ..MonitorControl::default() };
    let services = CompositorServices::start_with(bus.builder(), monitors, control).await.unwrap();
    let (events, rx) = mpsc::unbounded_channel();
    tokio::spawn(services.run(rx));
    Running { events, applied }
}

async fn proxy(bus: &TestBus) -> DisplayConfigProxy<'static> {
    let conn = bus.connect().await;
    DisplayConfigProxy::builder(&conn).cache_properties(CacheProperties::No).build().await.unwrap()
}

fn one(x: i32, primary: bool, connector: &'static str) -> ApplyLogical<'static> {
    (x, 0, 1.0, 0, primary, vec![(connector, FHD, HashMap::new())])
}

/// Connectors with a current mode, by x position.
async fn shown(dc: &DisplayConfigProxy<'_>) -> Vec<(String, i32, bool)> {
    let (_, _, logical, _) = dc.get_current_state().await.unwrap();
    let mut shown: Vec<_> = logical.into_iter().map(|l| (l.5[0].0.clone(), l.0, l.4)).collect();
    shown.sort_by_key(|s| s.1);
    shown
}

async fn eventually<F: std::future::Future<Output = bool>>(mut check: impl FnMut() -> F) {
    for _ in 0..100 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition never became true");
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("helix-apply-{name}-{}", std::process::id())).join("monitors.toml")
}

#[tokio::test(flavor = "multi_thread")]
async fn verify_temporary_and_rejections() {
    let Some(bus) = TestBus::start() else { return };
    let store = scratch("temp");
    let running = start(&bus, &store, both_on()).await;
    let dc = proxy(&bus).await;
    assert!(dc.apply_monitors_config_allowed().await.unwrap());
    let (serial, ..) = dc.get_current_state().await.unwrap();
    let swapped = [one(0, true, "HDMI-1"), one(1920, false, "DP-1")];
    let none = HashMap::new;

    // Verify changes nothing.
    dc.apply_monitors_config(serial, 0, &swapped, none()).await.unwrap();
    assert_eq!(*running.applied.lock().unwrap(), 0);
    assert_eq!(dc.get_current_state().await.unwrap().0, serial);

    // Stale serial, invalid layouts and unknown methods are refused.
    let err = dc.apply_monitors_config(serial + 1, 1, &swapped, none()).await.unwrap_err();
    assert!(err.to_string().contains("InvalidArgs") && err.to_string().contains("stale"), "{err}");
    let overlapping = [one(0, true, "HDMI-1"), one(100, false, "DP-1")];
    let err = dc.apply_monitors_config(serial, 1, &overlapping, none()).await.unwrap_err();
    assert!(err.to_string().contains("overlap"), "{err}");
    let err = dc.apply_monitors_config(serial, 3, &swapped, none()).await.unwrap_err();
    assert!(err.to_string().contains("unknown method"), "{err}");
    assert_eq!(*running.applied.lock().unwrap(), 0);

    // Temporary: applied and announced before the call returns; not stored.
    let mut changed = dc.receive_monitors_changed().await.unwrap();
    dc.apply_monitors_config(serial, 1, &swapped, none()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), changed.next()).await.expect("MonitorsChanged");
    let (new_serial, ..) = dc.get_current_state().await.unwrap();
    assert_ne!(new_serial, serial);
    assert_eq!(
        shown(&dc).await,
        [("HDMI-1".to_owned(), 0, true), ("DP-1".to_owned(), 1920, false)]
    );
    assert!(!store.exists(), "temporary configurations are not remembered");
    drop(running.events);
}

#[tokio::test(flavor = "multi_thread")]
async fn persistent_configuration_comes_back() {
    let Some(bus) = TestBus::start() else { return };
    let store = scratch("persist");
    let running = start(&bus, &store, both_on()).await;
    let dc = proxy(&bus).await;
    let (serial, ..) = dc.get_current_state().await.unwrap();

    // Persistent: only DP-1 stays on.
    dc.apply_monitors_config(serial, 2, &[one(0, true, "DP-1")], HashMap::new()).await.unwrap();
    assert_eq!(shown(&dc).await, [("DP-1".to_owned(), 0, true)]);
    assert!(store.exists());

    // Unplug HDMI-1 (no configuration remembered for DP-1 alone) …
    running.events.send(Event::Monitors(vec![monitor("DP-1", 0, true)])).unwrap();
    eventually(|| async { dc.get_current_state().await.unwrap().1.len() == 1 }).await;
    assert_eq!(*running.applied.lock().unwrap(), 1);
    // … and plug it back in, showing on both: the remembered setup returns.
    running.events.send(Event::Monitors(both_on())).unwrap();
    eventually(|| async { *running.applied.lock().unwrap() == 2 }).await;
    eventually(|| async { shown(&dc).await == [("DP-1".to_owned(), 0, true)] }).await;
    drop(running);

    // A new compositor with the same monitors restores it at start.
    let Some(bus) = TestBus::start() else { return };
    let running = start(&bus, &store, both_on()).await;
    let dc = proxy(&bus).await;
    eventually(|| async { shown(&dc).await == [("DP-1".to_owned(), 0, true)] }).await;
    assert_eq!(*running.applied.lock().unwrap(), 1);
    let _ = std::fs::remove_dir_all(store.parent().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_without_a_compositor_that_applies() {
    let Some(bus) = TestBus::start() else { return };
    let services = CompositorServices::start(bus.builder(), both_on()).await.unwrap();
    let (_events, rx) = mpsc::unbounded_channel();
    tokio::spawn(services.run(rx));
    let dc = proxy(&bus).await;
    assert!(!dc.apply_monitors_config_allowed().await.unwrap());
    let (serial, ..) = dc.get_current_state().await.unwrap();
    let err = dc
        .apply_monitors_config(serial, 1, &[one(0, true, "DP-1")], HashMap::new())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("AccessDenied"), "{err}");
}

/// The shell's display change question, answered by the test.
#[derive(Default)]
struct FakeDialog {
    calls: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.helix.DisplayChangeDialog")]
impl FakeDialog {
    async fn open(&self, seconds: u32) {
        self.calls.lock().unwrap().push(format!("Open {seconds}"));
    }
    async fn close(&self) {
        self.calls.lock().unwrap().push("Close".into());
    }
    #[zbus(signal)]
    async fn kept(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn reverted(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
}

const DIALOG_PATH: &str = "/org/helix/DisplayChangeDialog";

async fn fake_dialog(bus: &TestBus) -> (zbus::Connection, Arc<Mutex<Vec<String>>>) {
    let dialog = FakeDialog::default();
    let calls = dialog.calls.clone();
    let conn = bus
        .builder()
        .serve_at(DIALOG_PATH, dialog)
        .unwrap()
        .name("org.gnome.Shell")
        .unwrap()
        .build()
        .await
        .unwrap();
    (conn, calls)
}

async fn answer(conn: &zbus::Connection, keep: bool) {
    let emitter = zbus::object_server::SignalEmitter::new(conn, DIALOG_PATH).unwrap();
    if keep {
        FakeDialog::kept(&emitter).await.unwrap();
    } else {
        FakeDialog::reverted(&emitter).await.unwrap();
    }
}

fn dp_only() -> [ApplyLogical<'static>; 1] {
    [one(0, true, "DP-1")]
}

fn both() -> Vec<(String, i32, bool)> {
    vec![("DP-1".to_owned(), 0, true), ("HDMI-1".to_owned(), 1920, false)]
}

#[tokio::test(flavor = "multi_thread")]
async fn kept_changes_are_remembered() {
    let Some(bus) = TestBus::start() else { return };
    let store = scratch("keep");
    let (shell, calls) = fake_dialog(&bus).await;
    let _running = start(&bus, &store, both_on()).await;
    let dc = proxy(&bus).await;
    let (serial, ..) = dc.get_current_state().await.unwrap();

    dc.apply_monitors_config(serial, 2, &dp_only(), HashMap::new()).await.unwrap();
    assert_eq!(shown(&dc).await, [("DP-1".to_owned(), 0, true)], "applied before the answer");
    assert_eq!(*calls.lock().unwrap(), ["Open 20"]);
    assert!(!store.exists(), "not remembered until kept");

    answer(&shell, true).await;
    eventually(|| async { store.exists() }).await;
    assert_eq!(shown(&dc).await, [("DP-1".to_owned(), 0, true)]);
    let _ = std::fs::remove_dir_all(store.parent().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn reverted_and_unanswered_changes_go_back() {
    let Some(bus) = TestBus::start() else { return };
    let store = scratch("revert");
    let (shell, calls) = fake_dialog(&bus).await;
    let _running = start_confirming(&bus, &store, both_on(), 1).await;
    let dc = proxy(&bus).await;

    // Reverted by the user.
    let (serial, ..) = dc.get_current_state().await.unwrap();
    dc.apply_monitors_config(serial, 2, &dp_only(), HashMap::new()).await.unwrap();
    answer(&shell, false).await;
    eventually(|| async { shown(&dc).await == both() }).await;

    // Nobody answers: back after the countdown (1 s) and a grace period.
    let (serial, ..) = dc.get_current_state().await.unwrap();
    dc.apply_monitors_config(serial, 2, &dp_only(), HashMap::new()).await.unwrap();
    assert_eq!(shown(&dc).await.len(), 1);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(shown(&dc).await, both());
    assert_eq!(calls.lock().unwrap().last().map(String::as_str), Some("Close"), "the dialog is dismissed");
    assert!(!store.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_change_replaces_the_question() {
    let Some(bus) = TestBus::start() else { return };
    let store = scratch("second");
    let (shell, calls) = fake_dialog(&bus).await;
    let _running = start(&bus, &store, both_on()).await;
    let dc = proxy(&bus).await;

    let (serial, ..) = dc.get_current_state().await.unwrap();
    dc.apply_monitors_config(serial, 2, &dp_only(), HashMap::new()).await.unwrap();
    let (serial, ..) = dc.get_current_state().await.unwrap();
    let swapped = [one(0, true, "HDMI-1"), one(1920, false, "DP-1")];
    dc.apply_monitors_config(serial, 2, &swapped, HashMap::new()).await.unwrap();
    assert_eq!(*calls.lock().unwrap(), ["Open 20", "Close", "Open 20"]);

    // Reverting goes back to before both changes.
    answer(&shell, false).await;
    eventually(|| async { shown(&dc).await == both() }).await;
    assert!(!store.exists());
}

/// The BT.2100 color mode: offered for HDR monitors only, applied (and
/// remembered) when asked for; no HDR hardware is involved.
#[tokio::test(flavor = "multi_thread")]
async fn color_modes() {
    let Some(bus) = TestBus::start() else { return };
    let store = scratch("color");
    let mut hdr = monitor("DP-1", 0, true);
    hdr.hdr = true;
    let _running = start(&bus, &store, vec![hdr, monitor("HDMI-1", 1920, false)]).await;
    let dc = proxy(&bus).await;
    let (serial, monitors, ..) = dc.get_current_state().await.unwrap();
    let modes = |m: &WireMonitor| -> (u32, Vec<u32>) {
        (u32::try_from(m.2["color-mode"].clone()).unwrap(), Vec::<u32>::try_from(m.2["supported-color-modes"].clone()).unwrap())
    };
    assert_eq!(modes(&monitors[0]), (0, vec![0, 1]), "an HDR monitor");
    assert_eq!(modes(&monitors[1]), (0, vec![0]), "an SDR monitor");

    let hdr_props = || HashMap::from([("color-mode", Value::from(1u32))]);
    let config = |dp: HashMap<&'static str, Value<'static>>, hdmi: HashMap<&'static str, Value<'static>>| -> Vec<ApplyLogical<'static>> {
        vec![(0, 0, 1.0, 0, true, vec![("DP-1", FHD, dp)]), (1920, 0, 1.0, 0, false, vec![("HDMI-1", FHD, hdmi)])]
    };
    // An SDR monitor cannot be put in BT.2100.
    let refused = dc.apply_monitors_config(serial, 1, &config(HashMap::new(), hdr_props()), HashMap::new()).await;
    assert!(refused.is_err(), "{refused:?}");
    // The HDR one can.
    dc.apply_monitors_config(serial, 2, &config(hdr_props(), HashMap::new()), HashMap::new()).await.unwrap();
    let (_, monitors, ..) = dc.get_current_state().await.unwrap();
    assert_eq!(modes(&monitors[0]).0, 1);
    assert_eq!(modes(&monitors[1]).0, 0);
    let saved = std::fs::read_to_string(&store).unwrap();
    assert!(saved.contains("color_mode = 1"), "remembered: {saved}");
    let _ = std::fs::remove_dir_all(store.parent().unwrap());
}
