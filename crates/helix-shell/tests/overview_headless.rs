//! The window overview in a headless Helix session (helix-comp with
//! helix-shell, scratch home, private bus): opened and closed several
//! times, each drawing stays well under a frame budget on the UI thread
//! (the installed apps are read off it), and it is snapshotted.
//!
//! Needs helix-comp built next to this test; skipped otherwise.

use std::time::Duration;

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};

/// Open or close the overview (`OverviewActive`).
fn overview(session: &HeadlessSession, active: bool) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let conn = session.bus().connect().await;
        let value = zbus::zvariant::Value::from(active);
        conn.call_method(Some("org.gnome.Shell"), "/org/gnome/Shell", Some("org.freedesktop.DBus.Properties"), "Set", &("org.gnome.Shell", "OverviewActive", value))
            .await
            .expect("OverviewActive");
    });
}

/// The durations of the overview's drawings logged so far, in ms.
fn drawings(session: &HeadlessSession) -> Vec<f64> {
    session
        .log()
        .lines()
        .filter(|l| l.contains("overview drawn"))
        .filter_map(|l| l.split("ms=").nth(1)?.split_whitespace().next()?.parse().ok())
        .collect()
}

#[test]
fn overview_draws_quickly() {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return;
    };
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    let settings = "[org/gnome/desktop/interface]\nenable-hot-corners=false\n";
    let env = vec![("RUST_LOG".to_owned(), "info,helix_shell::ui=debug,helix_shell::watchdog=debug".to_owned())];
    let Some(session) = HeadlessSession::start(Options { comp: &comp, shell: Some(&shell), size: (1280, 720), settings, env }) else { return };
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());
    for round in 0..5 {
        overview(&session, true);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while drawings(&session).len() <= round {
            assert!(std::time::Instant::now() < deadline, "the overview was not drawn:\n{}", session.log());
            std::thread::sleep(Duration::from_millis(50));
        }
        if round == 0 {
            if let Some(dir) = std::env::var_os("HELIX_OVERVIEW_PNG_DIR") {
                session.keep_snapshot(&std::path::Path::new(&dir).join("overview.png"));
            }
        }
        overview(&session, false);
        std::thread::sleep(Duration::from_millis(300));
    }
    let times = drawings(&session);
    eprintln!("overview drawings (ms): {times:?}");
    let slowest = times.iter().cloned().fold(0.0, f64::max);
    // A debug build draws a 1280×720 overview in some 40 ms; shrinking the
    // wallpaper on the UI thread took 190 ms more here (over a second for
    // a large picture in a release build). Stalls like that are caught.
    assert!(slowest < 150.0, "a drawing took {slowest} ms: {times:?}");
}
