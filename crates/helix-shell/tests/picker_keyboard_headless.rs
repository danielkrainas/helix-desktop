//! Picking an area and a point with only the keyboard (helix-desktop#97),
//! end to end: a headless Helix session (helix-comp with helix-shell,
//! scratch home, private bus); this test asks for an area
//! (`SelectArea`) and a color (`PickColor`) as an allowed caller, then
//! answers with arrow keys and Enter through the test input.
//!
//! Needs helix-comp built next to this test; skipped otherwise.

use std::time::Duration;

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};

const PATH: &str = "/org/gnome/Shell/Screenshot";
const IFACE: &str = "org.gnome.Shell.Screenshot";

/// Wait until the picker's surface `n` times over is up.
fn picker_open(session: &HeadlessSession, n: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while session.log().matches("namespace=\"helix-screenshot\"").count() < n {
        assert!(std::time::Instant::now() < deadline, "the picker did not open:\n{}", session.log());
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(500));
}

#[test]
fn area_and_point_with_the_keyboard() {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return;
    };
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    let settings = "[org/gnome/desktop/interface]\nenable-hot-corners=false\n";
    let env = vec![("RUST_LOG".to_owned(), "info,helix_shell::ui::capture=debug".to_owned())];
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: Some(&shell), size: (1280, 720), settings, env }) else { return };
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let conn = rt.block_on(async {
        let conn = session.bus().connect().await;
        // gnome-screenshot's name: an allowed caller.
        conn.request_name("org.gnome.Screenshot").await.expect("the caller's name");
        conn
    });
    let call = |method: &'static str| {
        let conn = conn.clone();
        rt.spawn(async move { conn.call_method(Some("org.gnome.Shell"), PATH, Some(IFACE), method, &()).await })
    };

    // An area: the first arrow makes the middle quarter (320, 180,
    // 640 × 360), Right moves it 10 px, Ctrl+Right widens it 10 px.
    let area = call("SelectArea");
    picker_open(&session, 1);
    session.input("tap 106\ntap 106\ntap 29 106\ntap 28");
    let reply = rt.block_on(area).unwrap().expect("SelectArea answered");
    let picked: (i32, i32, i32, i32) = reply.body().deserialize().unwrap();
    assert_eq!(picked, (330, 180, 650, 360));

    // A point: the first arrow puts it in the middle, Shift+Left takes it
    // 50 px left; Enter picks it. (The color read there is not waited
    // for: the headless backend need not answer that capture.)
    let _color = call("PickColor");
    picker_open(&session, 2);
    session.input("tap 106\ntap 42 105\ntap 28");
    assert!(session.wait_for_log("picker closed result=Point(590, 360)", Duration::from_secs(10)), "the point picked:\n{}", session.log());
}
