//! `org.freedesktop.a11y.PointerLocator` (helixdesktop/desktop#75): a
//! screen reader asks what is under the pointer and hears once when it
//! next moves. A window that gave its accessibility names
//! (`gtk_surface1.set_a11y_properties`) is named by them, with the
//! pointer's place in its surface; over nothing the answer is empty, with
//! the place in the layout. Only the screen reader may ask.
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt;
use helix_test_support::headless::{HeadlessSession, Options};
use zbus::zvariant::OwnedValue;

#[zbus::proxy(
    interface = "org.freedesktop.a11y.PointerLocator",
    default_service = "org.freedesktop.a11y.Manager",
    default_path = "/org/freedesktop/a11y/Manager"
)]
trait PointerLocator {
    fn query_pointer(&self) -> zbus::Result<(HashMap<String, OwnedValue>, f64, f64)>;
    #[zbus(signal)]
    fn pointer_position_changed(&self) -> zbus::Result<()>;
}

fn text(v: &OwnedValue) -> String {
    match &**v {
        zbus::zvariant::Value::Str(s) => s.to_string(),
        zbus::zvariant::Value::ObjectPath(p) => p.to_string(),
        other => format!("{other:?}"),
    }
}

#[test]
fn screen_reader_locates_the_pointer() {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: None, size: (1280, 800), settings: "", env: Vec::new() }) else { return };
    let mut c = common::connect(&session);
    let (named, gtk) = c.toplevel((400, 300), |c, t| c.gtk_shell.get_gtk_surface(&t.surface, &c.qh, ()));
    gtk.set_a11y_properties(":1.42".into(), "/org/a11y/atspi/accessible/7".into());
    c.roundtrip();
    let (x, y) = common::point_over(&mut session, &mut c, &named.surface, (1280, 800));

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let bus = session.bus();
        // Anyone but the screen reader is turned away.
        let stranger = bus.connect().await;
        let err = PointerLocatorProxy::new(&stranger).await.unwrap().query_pointer().await.unwrap_err();
        assert!(matches!(err, zbus::Error::MethodError(ref name, _, _) if name.as_str() == "org.freedesktop.DBus.Error.AccessDenied"), "{err:?}");

        let orca = bus.connect().await;
        orca.request_name("org.gnome.Orca.KeyboardMonitor").await.unwrap();
        let locator = PointerLocatorProxy::new(&orca).await.unwrap();
        let mut moved = locator.receive_pointer_position_changed().await.unwrap();
        let (data, rx, ry) = locator.query_pointer().await.unwrap();
        assert_eq!(data.get("app-dbus-name").map(text).as_deref(), Some(":1.42"), "{data:?}");
        assert_eq!(data.get("toplevel-object-path").map(text).as_deref(), Some("/org/a11y/atspi/accessible/7"), "{data:?}");
        assert!((0.0..400.0).contains(&rx) && (0.0..300.0).contains(&ry), "relative to the window: {rx},{ry}");

        // One signal at the next motion, none after it until asked again.
        session.input(&format!("move {} {}", x + 5, y));
        tokio::time::timeout(Duration::from_secs(5), moved.next()).await.expect("PointerPositionChanged").unwrap();
        let (_, rx2, _) = locator.query_pointer().await.unwrap();
        assert!((rx2 - rx - 5.0).abs() < 0.01, "moved 5 to the right: {rx} → {rx2}");
        session.input(&format!("move {} {}", x + 6, y));
        tokio::time::timeout(Duration::from_secs(5), moved.next()).await.expect("PointerPositionChanged").unwrap();
        session.input(&format!("move {} {}", x + 7, y));
        assert!(tokio::time::timeout(Duration::from_millis(300), moved.next()).await.is_err(), "only one signal per query");

        // Over no window: nothing named, the place in the layout.
        let empty = (0..1280).step_by(40).flat_map(|x| (0..800).step_by(40).map(move |y| (x, y))).find(|(x, y)| {
            session.input(&format!("move {x} {y}"));
            c.roundtrip();
            c.state.pointer.is_none()
        });
        let (ex, ey) = empty.expect("somewhere without the window");
        let (data, gx, gy) = locator.query_pointer().await.unwrap();
        assert!(data.is_empty(), "{data:?}");
        assert_eq!((gx.round() as i32, gy.round() as i32), (ex, ey));
    });
    let log = session.log();
    assert!(!log.contains("panicked"), "{log}");
}

/// `gtk_surface1.titlebar_gesture` (version 5): a double click on GTK's
/// own title bar does what `action-double-click-titlebar` says (toggle
/// maximize by default), as on Helix's title bars.
#[test]
fn titlebar_double_click_maximizes() {
    use helix_protocols::gtk_shell_client::gtk_surface1::Gesture;
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: None, size: (1280, 800), settings: "", env: Vec::new() }) else { return };
    let mut c = common::connect(&session);
    let (w, gtk) = c.toplevel((400, 300), |c, t| c.gtk_shell.get_gtk_surface(&t.surface, &c.qh, ()));
    let (x, y) = common::point_over(&mut session, &mut c, &w.surface, (1280, 800));
    session.input(&format!("click {x} {y}"));
    c.roundtrip();
    let serial = c.state.press.expect("the click reached the window");
    gtk.titlebar_gesture(serial, &c.seat, Gesture::DoubleClick);
    c.until("not maximized by the double click", |s| s.maximized);
    gtk.titlebar_gesture(serial, &c.seat, Gesture::DoubleClick);
    c.until("not restored by the second double click", |s| !s.maximized);
}
