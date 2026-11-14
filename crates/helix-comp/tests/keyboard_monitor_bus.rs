//! `org.freedesktop.a11y.KeyboardMonitor` over a private bus: who may use
//! it, grabs reaching the compositor's side, key events going to their
//! clients only, and clients leaving.

use std::time::Duration;

use futures_util::StreamExt;
use helix_comp::keyboard_monitor::{self, Key, Shared};
use helix_comp::{CompositorServices, Event, MonitorControl};
use helix_test_support::TestBus;
use tokio::sync::mpsc;

#[zbus::proxy(
    interface = "org.freedesktop.a11y.KeyboardMonitor",
    default_service = "org.freedesktop.a11y.Manager",
    default_path = "/org/freedesktop/a11y/Manager"
)]
trait KeyboardMonitor {
    fn grab_keyboard(&self) -> zbus::Result<()>;
    fn ungrab_keyboard(&self) -> zbus::Result<()>;
    fn watch_keyboard(&self) -> zbus::Result<()>;
    fn unwatch_keyboard(&self) -> zbus::Result<()>;
    fn set_key_grabs(&self, modifiers: &[u32], keystrokes: &[(u32, u32)]) -> zbus::Result<()>;
    #[zbus(signal)]
    fn key_event(&self, released: bool, state: u32, keysym: u32, unichar: u32, keycode: u16) -> zbus::Result<()>;
}

const INSERT: u32 = 0xff63;
const KP_ADD: u32 = 0xffab;

async fn start(bus: &TestBus) -> (Shared, mpsc::UnboundedSender<Event>) {
    let shared = Shared::default();
    let control = MonitorControl { keyboard_monitor: shared.clone(), ..MonitorControl::default() };
    let services = CompositorServices::start_with(bus.builder(), Vec::new(), control).await.unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(services.run(rx));
    (shared, tx)
}

/// What the compositor does with a key: decide, and send the report.
fn key(shared: &Shared, events: &mpsc::UnboundedSender<Event>, key: Key) -> bool {
    let verdict = keyboard_monitor::lock(shared).key(&key, 0, 500, false);
    if let Some(report) = verdict.report {
        events.send(Event::KeyMonitor(report)).unwrap();
    }
    verdict.grabbed
}

fn press(keycode: u32, keysym: u32, unichar: u32) -> Key {
    Key { keycode, pressed: true, keysym, base_keysym: keysym, unichar, state: 0 }
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..100 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn screen_reader_watches_and_grabs() {
    let Some(bus) = TestBus::start() else { return };
    let (shared, events) = start(&bus).await;

    // Anyone else is turned away.
    let stranger = bus.connect().await;
    let stranger_monitor = KeyboardMonitorProxy::new(&stranger).await.unwrap();
    let err = stranger_monitor.watch_keyboard().await.unwrap_err();
    assert!(matches!(err, zbus::Error::MethodError(ref name, _, _) if name.as_str() == "org.freedesktop.DBus.Error.AccessDenied"), "{err:?}");
    assert!(stranger_monitor.grab_keyboard().await.is_err());
    assert!(stranger_monitor.set_key_grabs(&[INSERT], &[]).await.is_err());
    assert!(keyboard_monitor::lock(&shared).is_idle());
    let mut stranger_keys = stranger_monitor.receive_key_event().await.unwrap();

    // The screen reader, by its name.
    let orca = bus.connect().await;
    orca.request_name(keyboard_monitor::ALLOWED_CALLER).await.unwrap();
    let orca_name = orca.unique_name().unwrap().to_string();
    let monitor = KeyboardMonitorProxy::new(&orca).await.unwrap();
    let mut keys = monitor.receive_key_event().await.unwrap();
    monitor.watch_keyboard().await.unwrap();
    monitor.set_key_grabs(&[INSERT], &[(KP_ADD, 0)]).await.unwrap();

    // A plain key: the app gets it, the screen reader hears of it.
    let a = Key { state: 1, ..press(38, 'A' as u32, 'A' as u32) };
    assert!(!key(&shared, &events, a));
    let signal = tokio::time::timeout(Duration::from_secs(5), keys.next()).await.unwrap().unwrap();
    assert_eq!(signal.message().header().destination().map(|d| d.to_string()), Some(orca_name.clone()), "sent to the client only");
    let args = signal.args().unwrap();
    assert_eq!((args.released, args.state, args.keysym, args.unichar, args.keycode), (false, 1, 'A' as u32, 'A' as u32, 38));

    // Its modifier and keystroke are grabbed.
    assert!(key(&shared, &events, press(118, INSERT, 0)));
    assert!(key(&shared, &events, Key { pressed: false, ..press(118, INSERT, 0) }));
    assert!(key(&shared, &events, press(86, KP_ADD, '+' as u32)));
    let mut seen = Vec::new();
    for _ in 0..3 {
        let signal = tokio::time::timeout(Duration::from_secs(5), keys.next()).await.unwrap().unwrap();
        let args = signal.args().unwrap();
        seen.push((args.keysym, args.released));
    }
    assert_eq!(seen, [(INSERT, false), (INSERT, true), (KP_ADD, false)]);

    // The keyboard grab, and its end.
    monitor.grab_keyboard().await.unwrap();
    assert!(key(&shared, &events, press(39, 's' as u32, 's' as u32)));
    monitor.ungrab_keyboard().await.unwrap();
    assert!(!key(&shared, &events, press(40, 'd' as u32, 'd' as u32)));
    monitor.unwatch_keyboard().await.unwrap();
    assert!(!key(&shared, &events, press(41, 'f' as u32, 'f' as u32)));
    let mut seen = Vec::new();
    for _ in 0..2 {
        let signal = tokio::time::timeout(Duration::from_secs(5), keys.next()).await.unwrap().unwrap();
        seen.push(signal.args().unwrap().keysym);
    }
    assert_eq!(seen, ['s' as u32, 'd' as u32], "nothing after UnwatchKeyboard");
    assert!(tokio::time::timeout(Duration::from_millis(300), keys.next()).await.is_err());

    // The stranger heard nothing.
    assert!(tokio::time::timeout(Duration::from_millis(100), stranger_keys.next()).await.is_err());

    // Giving the name up stops new calls but keeps what is set up.
    orca.release_name(keyboard_monitor::ALLOWED_CALLER).await.unwrap();
    assert!(monitor.watch_keyboard().await.is_err());
    assert!(key(&shared, &events, press(118, INSERT, 0)));

    // Leaving the bus drops everything (the held key's release still
    // goes the way of its press).
    drop(keys);
    drop(monitor);
    drop(orca);
    eventually("the client to be dropped", || {
        let mut monitor = keyboard_monitor::lock(&shared);
        let mut probe = press(87, KP_ADD, '+' as u32);
        let grabbed = monitor.key(&probe, 0, 500, false);
        probe.pressed = false;
        monitor.key(&probe, 0, 500, false);
        !grabbed.grabbed
    })
    .await;
    for (code, sym) in [(118, INSERT), (86, KP_ADD), (39, 's' as u32)] {
        assert!(key(&shared, &events, Key { pressed: false, ..press(code, sym, 0) }), "grabbed presses are released grabbed");
    }
    assert!(keyboard_monitor::lock(&shared).is_idle());
}

#[tokio::test(flavor = "multi_thread")]
async fn matches_the_contract() {
    let Some(bus) = TestBus::start() else { return };
    let _services = start(&bus).await;
    let file = "org.freedesktop.a11y.KeyboardMonitor.xml";
    if !helix_test_support::conformance::spec_dir().join("contracts/dbus").join(file).exists() {
        eprintln!("skipping: {file} not in the spec checkout (set HELIX_SPEC_DIR)");
        return;
    }
    use helix_test_support::conformance::{assert_conformant, check, introspect, load_dbus_contract};
    let conn = bus.connect().await;
    let xml = introspect(&conn, keyboard_monitor::BUS_NAME, keyboard_monitor::OBJECT_PATH).await;
    let iface = "org.freedesktop.a11y.KeyboardMonitor";
    assert_conformant(iface, &check(iface, &load_dbus_contract(file), &xml, &[]));
}
