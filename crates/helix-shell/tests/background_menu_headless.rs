//! The desktop background's menu end to end (helix-desktop-spec
//! `contracts/wayland/README.md`, "Background menu"): in a headless Helix
//! session (helix-comp with helix-shell, scratch home, private bus) a
//! right click on the background opens "Change Background…", "Display
//! Settings", "Settings" at the pointer, and choosing one starts the
//! settings app at that panel; a long press of the left button or a
//! finger opens it too, a short click does not; it is used with the
//! keyboard; a right click on the panel opens the panel's own menu
//! instead, and with desktop icons shown one on an icon opens none while
//! one beside them opens the background's.
//!
//! The settings app is a stand-in `helix-settings-app` first on `PATH`
//! that writes its arguments to a file. The menu's entries are found from
//! the shell's debug log ("menu shown"). With
//! `HELIX_BACKGROUND_MENU_PNG_DIR` the snapshot with the menu open is
//! kept there. Needs helix-comp built next to this test; skipped
//! otherwise.

use std::path::Path;
use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};

/// The entries of the `nth` (0-based) "menu shown" line, waiting for it:
/// (label, center).
fn menu_shown(session: &HeadlessSession, nth: usize) -> Vec<(String, (i32, i32))> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let log = session.log();
        if let Some(line) = log.lines().filter(|l| l.contains("menu shown")).nth(nth) {
            return line
                .split_whitespace()
                .map(|w| w.trim_matches(|c| c == '"' || c == '\''))
                .map(|w| w.strip_prefix("entries=").unwrap_or(w).trim_matches('"'))
                .filter_map(|w| {
                    let (label, rect) = w.split_once('@')?;
                    let n: Vec<i32> = rect.split(',').map(|v| v.parse().ok()).collect::<Option<_>>()?;
                    (n.len() == 4).then(|| (label.replace('_', " "), (n[0] + n[2] / 2, n[1] + n[3] / 2)))
                })
                .collect();
        }
        assert!(Instant::now() < deadline, "no menu shown ({nth}):\n{log}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn shown_count(session: &HeadlessSession) -> usize {
    session.log().lines().filter(|l| l.contains("menu shown")).count()
}

/// Until the stand-in settings app has been started with each of `want`.
fn started(marker: &Path, want: &[&str], session: &HeadlessSession) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let got = std::fs::read_to_string(marker).unwrap_or_default();
        if got.lines().collect::<Vec<_>>() == want {
            return;
        }
        assert!(Instant::now() < deadline, "started {got:?}, wanted {want:?}:\n{}", session.log());
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn labels(entries: &[(String, (i32, i32))]) -> Vec<&str> {
    entries.iter().map(|(l, _)| l.as_str()).collect()
}

fn center(entries: &[(String, (i32, i32))], label: &str) -> (i32, i32) {
    entries.iter().find(|(l, _)| l == label).unwrap_or_else(|| panic!("no {label} in {entries:?}")).1
}

#[test]
fn background_menu() {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return;
    };
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    let scratch = std::env::temp_dir().join(format!("helix-bg-menu-{}", std::process::id()));
    let bin = scratch.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let marker = scratch.join("started");
    let fake = bin.join("helix-settings-app");
    std::fs::write(&fake, format!("#!/bin/sh\necho settings \"$@\" >> {}\n", marker.display())).unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let settings = "[org/gnome/desktop/interface]\nenable-hot-corners=false\n\n[org/gnome/desktop/background]\nshow-desktop-icons=true\n";
    let env = vec![
        ("RUST_LOG".to_owned(), "info,helix_shell::ui::window_menu=debug,helix_shell::ui::background_menu=debug".to_owned()),
        ("PATH".to_owned(), format!("{}:/usr/local/bin:/usr/bin:/bin", bin.display())),
    ];
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: Some(&shell), size: (1280, 800), settings, env }) else { return };
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());
    std::thread::sleep(Duration::from_millis(800));
    let (w, _, before) = session.fresh_snapshot();

    // A right click: the menu at the pointer, GNOME's entries.
    session.input("move 600 360\nbutton 273 1\nbutton 273 0");
    let entries = menu_shown(&session, 0);
    assert_eq!(labels(&entries), ["Change Background…", "Display Settings", "Settings"], "{entries:?}");
    let (x, y) = center(&entries, "Change Background…");
    assert!((600..900).contains(&x) && (360..400).contains(&y), "below and right of the point: {entries:?}");
    // On screen: the menu box covers what was background.
    std::thread::sleep(Duration::from_millis(300));
    let (_, _, open) = session.fresh_snapshot();
    if let Some(dir) = std::env::var_os("HELIX_BACKGROUND_MENU_PNG_DIR") {
        let _ = std::fs::copy(session.snapshot_path(), Path::new(&dir).join("background-menu.png"));
    }
    let px = |data: &[u8], x: i32, y: i32| data[((y as u32 * w + x as u32) * 4) as usize..][..3].to_vec();
    let (dx, dy) = center(&entries, "Display Settings");
    assert_ne!(px(&before, dx + 60, dy), px(&open, dx + 60, dy), "the menu is drawn");
    session.input(&format!("click {dx} {dy}"));
    started(&marker, &["settings display"], &session);

    // A short left click on the background: nothing.
    session.input("click 300 300");
    std::thread::sleep(Duration::from_millis(900));
    assert_eq!(shown_count(&session), 1, "a click is not a long press:\n{}", session.log());

    // A long left press: the menu; "Settings" opens Settings itself.
    session.input("move 300 300\nbutton 272 1");
    let entries = menu_shown(&session, 1);
    session.input("button 272 0");
    assert_eq!(labels(&entries).len(), 3);
    let (sx, sy) = center(&entries, "Settings");
    session.input(&format!("click {sx} {sy}"));
    started(&marker, &["settings display", "settings"], &session);

    // A long press of a finger, then the keyboard: Down selects the first
    // entry, Return chooses it.
    session.input("touch down 700 500");
    let entries = menu_shown(&session, 2);
    session.input("touch up");
    assert_eq!(labels(&entries).len(), 3);
    std::thread::sleep(Duration::from_millis(200));
    session.input("keysym 0xff54 1\nkeysym 0xff54 0\nkeysym 0xff0d 1\nkeysym 0xff0d 0");
    started(&marker, &["settings display", "settings", "settings background"], &session);

    // A finger that moves away is not a long press.
    session.input("touch down 700 500\ntouch move 740 500");
    std::thread::sleep(Duration::from_millis(900));
    session.input("touch up");
    assert_eq!(shown_count(&session), 3, "a moved finger is not a long press:\n{}", session.log());

    // Escape closes it without starting anything.
    session.input("move 640 400\nbutton 273 1\nbutton 273 0");
    menu_shown(&session, 3);
    session.input("keysym 0xff1b 1\nkeysym 0xff1b 0");
    std::thread::sleep(Duration::from_millis(300));

    // On the panel, the panel's own menu.
    session.input("move 400 10\nbutton 273 1\nbutton 273 0");
    let entries = menu_shown(&session, 4);
    assert!(!labels(&entries).contains(&"Change Background…"), "the panel's menu, not the background's: {entries:?}");
    std::thread::sleep(Duration::from_millis(500));
    session.input("keysym 0xff1b 1\nkeysym 0xff1b 0");

    // With desktop icons: a right click on an icon opens nothing, one
    // beside the icons the background's menu.
    std::fs::create_dir_all(session.home().join("Desktop")).unwrap();
    std::fs::write(session.home().join("Desktop/notes.txt"), "notes").unwrap();
    assert!(session.wait_for_log("namespace=\"helix-background-icons\"", Duration::from_secs(10)), "the desktop icons:\n{}", session.log());
    // (The folder is looked at every two seconds.)
    std::thread::sleep(Duration::from_millis(3000));
    // The first cell: 12 pixels in from the icons' surface, under the panel.
    session.input("move 60 100\nbutton 273 1\nbutton 273 0");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(shown_count(&session), 5, "no menu on an icon:\n{}", session.log());
    session.input("move 60 500\nbutton 273 1\nbutton 273 0");
    let entries = menu_shown(&session, 5);
    assert_eq!(labels(&entries), ["Change Background…", "Display Settings", "Settings"], "beside the icons: {entries:?}");
    session.input("keysym 0xff1b 1\nkeysym 0xff1b 0");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 3, "nothing else started");
    let _ = std::fs::remove_dir_all(&scratch);
}
