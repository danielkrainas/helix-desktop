//! The app grid's "Recent" row end to end (helix-desktop-spec
//! `contracts/wayland/README.md`, "App grid", "Recent row"): in a headless
//! Helix session (helix-comp with helix-shell, scratch home, private bus)
//! two apps are launched by clicking them in the grid; the grid opened
//! again starts with a "Recent" row holding both, the latest first, and
//! clicking the older one there launches it.
//!
//! The apps are desktop files of the test's own (each appends its name to
//! a file when started); the cells are found from the shell's debug log
//! ("app grid shown"). Needs helix-comp built next to this test; skipped
//! otherwise.

use std::path::Path;
use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};

const ONE: &str = "helix-recent-one.desktop";
const TWO: &str = "helix-recent-two.desktop";

fn call(session: &HeadlessSession, interface: &str, method: &str) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let conn = session.bus().connect().await;
        conn.call_method(Some("org.gnome.Shell"), "/org/gnome/Shell", Some(interface), method, &()).await.expect(method);
    });
}

/// Until the shell says the launcher (overview) is closed.
fn closed(session: &HeadlessSession) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let conn = session.bus().connect().await;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let reply = conn
                .call_method(Some("org.gnome.Shell"), "/org/gnome/Shell", Some("org.freedesktop.DBus.Properties"), "Get", &("org.gnome.Shell", "OverviewActive"))
                .await
                .expect("OverviewActive");
            let value: zbus::zvariant::OwnedValue = reply.body().deserialize().expect("a variant");
            if !bool::try_from(value).expect("a boolean") {
                return;
            }
            assert!(Instant::now() < deadline, "the launcher stayed open");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
}

/// The cells of the `nth` (0-based) "app grid shown" line, waiting for
/// it: (key, center).
fn grid_shown(session: &HeadlessSession, nth: usize) -> Vec<(String, (i32, i32))> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let log = session.log();
        if let Some(line) = log.lines().filter(|l| l.contains("app grid shown")).nth(nth) {
            return line
                .split_whitespace()
                .map(|w| w.trim_matches(|c| c == '"' || c == '\''))
                .map(|w| w.strip_prefix("cells=").unwrap_or(w).trim_matches('"'))
                .filter_map(|w| {
                    let (key, rect) = w.split_once('@')?;
                    let n: Vec<i32> = rect.split(',').map(|v| v.parse().ok()).collect::<Option<_>>()?;
                    (n.len() == 4).then(|| (key.to_owned(), (n[0] + n[2] / 2, n[1] + n[3] / 2)))
                })
                .collect();
        }
        assert!(Instant::now() < deadline, "the grid was not shown ({nth}):\n{log}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn shown_count(session: &HeadlessSession) -> usize {
    session.log().lines().filter(|l| l.contains("app grid shown")).count()
}

/// Until `marker` holds `want` lines.
fn launched(marker: &Path, want: &[&str], session: &HeadlessSession) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let got = std::fs::read_to_string(marker).unwrap_or_default();
        if got.lines().collect::<Vec<_>>() == want {
            return;
        }
        assert!(Instant::now() < deadline, "launched {got:?}, wanted {want:?}:\n{}", session.log());
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Open the launcher and click cell `key`.
fn open_and_click(session: &mut HeadlessSession, key: &str) -> Vec<(String, (i32, i32))> {
    closed(session);
    let n = shown_count(session);
    call(session, "org.gnome.Shell", "ShowApplications");
    let cells = grid_shown(session, n);
    let Some((_, (x, y))) = cells.iter().find(|(k, _)| k == key) else { panic!("no cell {key} in {cells:?}") };
    // Let the launcher settle before the click (its first frame).
    std::thread::sleep(Duration::from_millis(300));
    session.input(&format!("click {x} {y}"));
    cells
}

#[test]
fn recent_row_shows_launched_apps() {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return;
    };
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    // The test's apps, in a data directory of their own.
    let share = std::env::temp_dir().join(format!("helix-grid-recent-{}", std::process::id()));
    let apps = share.join("applications");
    std::fs::create_dir_all(&apps).unwrap();
    let marker = share.join("launched");
    for (file, name, word) in [(ONE, "Recent Test One", "one"), (TWO, "Recent Test Two", "two")] {
        let entry = format!("[Desktop Entry]\nType=Application\nName={name}\nExec=sh -c \"echo {word} >> {}\"\nTerminal=false\n", marker.display());
        std::fs::write(apps.join(file), entry).unwrap();
    }
    // Both on the first page, before everything else.
    let settings = format!(
        "[org/gnome/desktop/interface]\nenable-hot-corners=false\n\n[org/gnome/shell]\napp-picker-layout=[{{'{ONE}': <{{'position': <0>}}>, '{TWO}': <{{'position': <1>}}>}}]\n"
    );
    let env = vec![
        ("RUST_LOG".to_owned(), "info,helix_shell::ui::app_grid=debug".to_owned()),
        ("XDG_DATA_DIRS".to_owned(), format!("{}:/usr/local/share:/usr/share", share.display())),
    ];
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: Some(&shell), size: (1280, 800), settings: &settings, env }) else { return };
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());
    std::thread::sleep(Duration::from_millis(500));

    // Nothing launched yet: no row.
    let cells = open_and_click(&mut session, ONE);
    assert!(!cells.iter().any(|(k, _)| k.starts_with("recent:")), "no recent apps yet: {cells:?}");
    launched(&marker, &["one"], &session);
    let cells = open_and_click(&mut session, TWO);
    assert_eq!(cells.iter().filter(|(k, _)| k.starts_with("recent:")).map(|(k, _)| k.as_str()).collect::<Vec<_>>(), [format!("recent:{ONE}")]);
    launched(&marker, &["one", "two"], &session);

    // The row: both, the latest first, before the grid (where they stay).
    let cells = open_and_click(&mut session, &format!("recent:{ONE}"));
    let keys: Vec<&str> = cells.iter().map(|(k, _)| k.as_str()).collect();
    eprintln!("the grid with its recent row: {:?}", &keys[..4.min(keys.len())]);
    assert_eq!(keys[..2], [format!("recent:{TWO}"), format!("recent:{ONE}")], "{keys:?}");
    assert!(keys[2..].contains(&ONE) && keys[2..].contains(&TWO), "the apps keep their grid places: {keys:?}");
    let (row_y, grid_y) = (cells[0].1 .1, cells[2].1 .1);
    assert!(row_y < grid_y, "the row is above the grid");
    // Clicked in the row: launched.
    launched(&marker, &["one", "two", "one"], &session);
    let log = session.log();
    let from_row = log.lines().filter(|l| l.contains("launching from the app grid") && l.contains("recent=true")).count();
    assert_eq!(from_row, 1, "launched from the row once:\n{log}");
    let recent = std::fs::read_to_string(session.home().join(".local/share/helix/recent-apps")).unwrap_or_default();
    assert_eq!(recent.lines().collect::<Vec<_>>(), [ONE, TWO], "kept in the scratch home");
    let _ = std::fs::remove_dir_all(&share);
}
