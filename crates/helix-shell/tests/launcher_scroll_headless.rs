//! The launcher's results scroll with the pointer (helixdesktop/desktop#104),
//! end to end: a headless Helix session (helix-comp with helix-shell,
//! scratch home, private bus) at Large Text (text scale 1.5), with thirty
//! apps of the test's own that match a search. Touchpad scrolling over
//! the results moves the list and lights the row now under the pointer;
//! the keyboard's selection brings the list back to it; the list never
//! scrolls past its ends. (The shell logs each drawing of the results:
//! "launcher results start=… above=… below=… hover=…".)
//!
//! Needs helix-comp built next to this test; skipped otherwise.

use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};

const SIZE: (u32, u32) = (1280, 720);
const APPS: usize = 30;

/// The latest drawing of the results: (start, above, below, hover).
fn latest(session: &HeadlessSession) -> Option<(usize, usize, usize, String)> {
    let log = session.log();
    let line = log.lines().rev().find(|l| l.contains("launcher results"))?;
    let field = |name: &str| line.split(&format!("{name}=")).nth(1).and_then(|v| v.split_whitespace().next()).map(str::to_owned);
    Some((field("start")?.parse().ok()?, field("above")?.parse().ok()?, field("below")?.parse().ok()?, field("hover")?))
}

/// Wait until the latest drawing satisfies `ok`.
fn until(session: &HeadlessSession, what: &str, ok: impl Fn(&(usize, usize, usize, String)) -> bool) -> (usize, usize, usize, String) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(found) = latest(session).filter(|f| ok(f)) {
            // Let a second drawing (the row under the pointer) come.
            std::thread::sleep(Duration::from_millis(200));
            return latest(session).unwrap_or(found);
        }
        assert!(Instant::now() < deadline, "{what}: {:?}\n{}", latest(session), session.log());
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn results_scroll_with_the_pointer() {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return;
    };
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    // The test's apps, found before the system's.
    let data = std::env::temp_dir().join(format!("helix-launcher-scroll-{}", std::process::id()));
    let apps = data.join("applications");
    std::fs::create_dir_all(&apps).unwrap();
    for i in 0..APPS {
        std::fs::write(apps.join(format!("org.example.Zqx{i:02}.desktop")), format!("[Desktop Entry]\nType=Application\nName=Zqxwv Note {i:02}\nExec=true\n")).unwrap();
    }
    let settings = "[org/gnome/desktop/interface]\nenable-hot-corners=false\nenable-animations=false\ntext-scaling-factor=1.5\n";
    let env = vec![
        ("RUST_LOG".to_owned(), "info,helix_shell::ui=debug".to_owned()),
        ("XDG_DATA_DIRS".to_owned(), format!("{}:/usr/local/share:/usr/share", data.display())),
    ];
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: Some(&shell), size: SIZE, settings, env }) else {
        let _ = std::fs::remove_dir_all(&data);
        return;
    };
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let conn = session.bus().connect().await;
        conn.call_method(Some("org.gnome.Shell"), "/org/gnome/Shell", Some("org.gnome.Shell"), "ShowApplications", &()).await.expect("ShowApplications");
    });
    assert!(session.wait_for_log("namespace=\"helix-launcher\"", Duration::from_secs(10)), "the launcher:\n{}", session.log());
    std::thread::sleep(Duration::from_millis(500));
    session.input("type zqxwv");
    let first = until(&session, "all thirty found, more below", |(start, above, below, _)| *start == 0 && *above == 0 && *below > 0);
    assert!(first.2 < APPS, "{first:?}");

    // The pointer over the list, then two rows' worth of touchpad travel
    // (rows are 72 px at Large Text).
    let x = SIZE.0 / 2;
    session.input(&format!("move {x} 420"));
    let before = until(&session, "a row lit under the pointer", |(_, _, _, hover)| hover.starts_with("Some"));
    session.input("scroll 0 150");
    let scrolled = until(&session, "scrolled down", |(start, _, _, _)| *start == 2);
    assert!(scrolled.1 > 0 && scrolled.2 < first.2, "results above now, fewer below: {scrolled:?}");
    assert!(scrolled.3.starts_with("Some") && scrolled.3 != before.3, "the row now under the pointer is lit: {before:?} → {scrolled:?}");

    // Far down: the end, nothing below.
    session.input("scroll 0 5000");
    let end = until(&session, "at the end", |(_, _, below, _)| *below == 0);
    assert!(end.1 > 0, "{end:?}");
    // The keyboard's selection (the first row, out of sight) moves to the
    // second: the list follows it.
    session.input("tap 108");
    let followed = until(&session, "back to the selection", |(start, _, _, _)| *start <= 2);
    assert!(followed.0 <= 2, "{followed:?}");
    // Up past the top.
    session.input("scroll 0 -5000");
    until(&session, "at the top", |(start, above, _, _)| *start == 0 && *above == 0);
    drop(session);
    let _ = std::fs::remove_dir_all(&data);
}
