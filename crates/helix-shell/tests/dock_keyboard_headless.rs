//! The dock from the keyboard (helixdesktop/desktop#39), in a headless Helix
//! session (helix-comp with helix-shell, scratch home, private bus):
//! `switch-panels` (Ctrl+Alt+Tab) goes to the panel, then to the dock,
//! which shows although it auto-hides; Right moves to the next item and
//! Enter launches it; Escape gives the keyboard back and the dock hides
//! again.
//!
//! Needs helix-comp built next to this test; skipped otherwise.

use std::path::Path;
use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};

/// Two harmless apps (each touches a file in the scratch home).
fn apps(home: &Path) {
    let dir = home.join(".local/share/applications");
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["first", "second"] {
        let entry = format!("[Desktop Entry]\nType=Application\nName=Test {name}\nExec=sh -c 'touch \"$HOME/launched-{name}\"'\nIcon=application-x-executable\n");
        std::fs::write(dir.join(format!("org.example.Dock{name}.desktop")), entry).unwrap();
    }
}

/// The dock's last "shown" state in the log.
fn dock_shown(log: &str) -> Option<bool> {
    log.lines().rev().find(|l| l.contains(" dock") && l.contains("shown=")).map(|l| l.contains("shown=true"))
}

#[test]
fn switch_panels_reaches_the_dock() {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return;
    };
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    let settings = "[org/gnome/desktop/interface]\nenable-hot-corners=false\nenable-animations=false\n\n[org/gnome/shell]\nfavorite-apps=['org.example.Dockfirst.desktop', 'org.example.Docksecond.desktop']\n\n[org/gnome/shell/extensions/dash-to-dock]\ndock-fixed=false\nautohide=true\nintellihide=false\nshow-trash=false\nshow-mounts=false\nshow-show-apps-button=false\n";
    let env = vec![("RUST_LOG".to_owned(), "info,helix_shell::ui::dock=debug".to_owned())];
    let Some(mut session) = HeadlessSession::start_prepared(Options { comp: &comp, shell: Some(&shell), size: (1280, 800), settings, env }, apps) else { return };
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());
    let deadline = Instant::now() + Duration::from_secs(10);
    while dock_shown(&session.log()) != Some(false) {
        assert!(Instant::now() < deadline, "the dock never hid (autohide):\n{}", session.log());
        std::thread::sleep(Duration::from_millis(100));
    }
    // The panel, then the dock: shown while it has the keyboard.
    session.input("tap 29 56 15");
    std::thread::sleep(Duration::from_millis(300));
    session.input("tap 29 56 15");
    let deadline = Instant::now() + Duration::from_secs(5);
    while dock_shown(&session.log()) != Some(true) {
        assert!(Instant::now() < deadline, "the dock did not show for the keyboard:\n{}", session.log());
        std::thread::sleep(Duration::from_millis(100));
    }
    // Right, Enter: the second app.
    session.input("tap 106");
    std::thread::sleep(Duration::from_millis(200));
    session.input("tap 28");
    assert!(session.wait_for_log("launching from the dock app=\"org.example.Docksecond.desktop\"", Duration::from_secs(5)), "Enter launched the second item:\n{}", session.log());
    let launched = session.home().join("launched-second");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !launched.exists() {
        assert!(Instant::now() < deadline, "the app did not run");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!session.home().join("launched-first").exists(), "only the chosen item");
    // Back to the dock, then Escape: it hides again.
    session.input("tap 29 56 15");
    std::thread::sleep(Duration::from_millis(300));
    session.input("tap 29 56 15");
    std::thread::sleep(Duration::from_millis(300));
    session.input("tap 1");
    let deadline = Instant::now() + Duration::from_secs(5);
    while dock_shown(&session.log()) != Some(false) {
        assert!(Instant::now() < deadline, "the dock did not hide after Escape:\n{}", session.log());
        std::thread::sleep(Duration::from_millis(100));
    }
}
