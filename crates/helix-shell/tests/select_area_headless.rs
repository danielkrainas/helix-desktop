//! `SelectArea` as the Screenshot app (helix-apps `apps/screenshot`) calls
//! it, answered with the pointer, end to end (danielkrainas/helix-apps#146):
//! a headless Helix session (helix-comp with helix-shell, scratch home,
//! private bus); this test claims `org.gnome.Screenshot` without queueing,
//! asks for an area, sees the picker dim the screen, drags out a rectangle
//! with the test input and gets it back.
//!
//! Needs helix-comp built next to this test; skipped otherwise.

use std::time::Duration;

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};

const PATH: &str = "/org/gnome/Shell/Screenshot";
const IFACE: &str = "org.gnome.Shell.Screenshot";

/// The mean brightness (0–255) of an RGBA snapshot.
fn brightness((_, _, rgba): &(u32, u32, Vec<u8>)) -> f64 {
    let sum: u64 = rgba.as_chunks::<4>().0.iter().map(|p| u64::from(p[0]) + u64::from(p[1]) + u64::from(p[2])).sum();
    sum as f64 / (rgba.len() / 4 * 3) as f64
}

#[test]
fn select_area_with_the_pointer() {
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
        // As the app: the caller's name, never queued.
        let reply = conn.request_name_with_flags("org.gnome.Screenshot", zbus::fdo::RequestNameFlags::DoNotQueue.into()).await.expect("the caller's name");
        assert_eq!(reply, zbus::fdo::RequestNameReply::PrimaryOwner);
        conn
    });
    std::thread::sleep(Duration::from_millis(500));
    let before = brightness(&session.fresh_snapshot());

    let area = {
        let conn = conn.clone();
        rt.spawn(async move { conn.call_method(Some("org.gnome.Shell"), PATH, Some(IFACE), "SelectArea", &()).await })
    };
    assert!(session.wait_for_log("namespace=\"helix-screenshot\"", Duration::from_secs(10)), "the picker did not open:\n{}", session.log());
    std::thread::sleep(Duration::from_millis(500));
    // The picker is painted: its scrim dims the whole screen.
    let during = brightness(&session.fresh_snapshot());
    assert!(during < before * 0.75, "the picker is not shown (brightness {before:.0} before, {during:.0} with it)");

    // A drag from (100, 100) to (300, 250).
    session.input("move 100 100\nbutton 272 1\nmove 200 150\nmove 300 250\nbutton 272 0");
    let reply = rt.block_on(area).unwrap().expect("SelectArea answered");
    let picked: (i32, i32, i32, i32) = reply.body().deserialize().unwrap();
    assert_eq!(picked, (100, 100, 200, 150));
}
