//! High contrast on the live surfaces, end to end: a headless Helix
//! session (helix-comp with helix-shell, scratch home, private bus) with
//! `high-contrast` on and two X11 windows (xclock and xeyes on the
//! session's own Xwayland), snapshotted with the application switcher
//! open (Alt held, Tab) and with the overview open:
//!
//! - the switcher is a black box with a white edge, nothing but black,
//!   white and greys around its icons;
//! - the overview's search field is outlined in white, the active
//!   workspace in the accent, and the windows on a workspace (no
//!   pictures in a headless session) are outlined in white, not black on
//!   black.
//!
//! With `HELIX_HC_PNG_DIR=dir` the snapshots are kept there
//! (`hc-desktop.png`, `hc-switcher.png`, `hc-overview.png`).
//!
//! Needs helix-comp built next to this test, Xwayland, xclock and xeyes;
//! skipped otherwise.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};

const SIZE: (u32, u32) = (1280, 720);

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

/// An X11 client on the session's Xwayland, in its scratch home.
fn x_client(session: &HeadlessSession, program: &str, args: &[&str]) -> Option<Child> {
    Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", session.home())
        .env("DISPLAY", &session.x_display)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

/// The pixel at (x, y) of an RGBA snapshot `w` wide.
fn rgb(data: &[u8], w: u32, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * w + x) * 4) as usize;
    [data[i], data[i + 1], data[i + 2]]
}

/// The box around every pixel that differs between two snapshots.
fn changed(a: &[u8], b: &[u8], w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for y in 0..h {
        for x in 0..w {
            if rgb(a, w, x, y) != rgb(b, w, x, y) {
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            }
        }
    }
    (x1 >= x0 && y1 >= y0).then(|| (x0, y0, x1 - x0 + 1, y1 - y0 + 1))
}

/// Black, white or a grey between them (text and lines anti-aliased on
/// black), or the high-contrast accent (blue's fill) and its blends.
fn high_contrast_color([r, g, b]: [u8; 3]) -> bool {
    let grey = r.abs_diff(g) < 12 && g.abs_diff(b) < 12;
    // Blue's fill #2965AF mixed with black or white keeps b ≥ g ≥ r.
    let accent = b >= g && g >= r && b.saturating_sub(r) > 40;
    grey || accent
}

/// How much of the box (x, y, w, h) is not a high-contrast color, and
/// its most common color.
fn survey(data: &[u8], width: u32, (x, y, w, h): (u32, u32, u32, u32)) -> (f64, [u8; 3]) {
    let mut counts = std::collections::HashMap::new();
    let mut other = 0usize;
    for py in y..y + h {
        for px in x..x + w {
            let c = rgb(data, width, px, py);
            *counts.entry(c).or_insert(0usize) += 1;
            if !high_contrast_color(c) {
                other += 1;
            }
        }
    }
    let top = counts.into_iter().max_by_key(|(_, n)| *n).map(|(c, _)| c).unwrap_or_default();
    (other as f64 / f64::from(w * h), top)
}

#[test]
fn switcher_and_overview_in_high_contrast() {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return;
    };
    if !["/usr/bin/xclock", "/usr/bin/xeyes"].iter().all(|p| std::path::Path::new(p).exists()) {
        eprintln!("xclock or xeyes is not installed; skipped");
        return;
    }
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    let settings = "[org/gnome/desktop/interface]\nenable-hot-corners=false\nenable-animations=false\n\n[org/gnome/desktop/a11y/interface]\nhigh-contrast=true\n";
    let env = vec![("RUST_LOG".to_owned(), "info,helix_shell::ui=debug".to_owned())];
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: Some(&shell), size: SIZE, settings, env }) else { return };
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());
    let keep = |session: &HeadlessSession, name: &str| {
        if let Some(dir) = std::env::var_os("HELIX_HC_PNG_DIR") {
            let dir = std::path::PathBuf::from(dir);
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::copy(session.snapshot_path(), dir.join(name));
        }
    };
    let (w, h) = SIZE;

    std::thread::sleep(Duration::from_secs(2));
    // Two windows that change neither on their own nor with focus.
    let mut clients: Vec<Child> = [x_client(&session, "xclock", &[]), x_client(&session, "xeyes", &[])].into_iter().flatten().collect();
    assert_eq!(clients.len(), 2, "xclock and xeyes started");
    // Shown (centered; Xwayland starts with the first X11 client): the
    // clock's white face near the middle of the screen.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let (_, _, now) = session.fresh_snapshot();
        if rgb(&now, w, w / 2 - 60, h / 2 + 20) == [255, 255, 255] {
            break;
        }
        assert!(Instant::now() < deadline, "the X11 windows did not show:\n{}", session.log());
    }
    std::thread::sleep(Duration::from_secs(2));
    let (_, _, desktop) = session.fresh_snapshot();
    keep(&session, "hc-desktop.png");

    // The application switcher: Alt held, Tab.
    session.input("key 56 1\ntap 15");
    std::thread::sleep(Duration::from_millis(600));
    let (_, _, switcher) = session.fresh_snapshot();
    keep(&session, "hc-switcher.png");
    session.input("key 56 0");
    // (Between the panel and the dock, which keep their places.)
    let (top_y, bottom_y) = (40u32, h - 100);
    let band = |data: &[u8]| data[(top_y * w * 4) as usize..(bottom_y * w * 4) as usize].to_vec();
    let area = changed(&band(&desktop), &band(&switcher), w, bottom_y - top_y).map(|(x, y, bw, bh)| (x, y + top_y, bw, bh)).expect("the switcher showed");
    eprintln!("the switcher at {area:?}");
    assert!(area.2 > 150 && area.3 > 100, "a switcher, not a speck: {area:?}");
    let (x, y, bw, bh) = area;
    let (_, top) = survey(&switcher, w, area);
    assert_eq!(top, [0, 0, 0], "the switcher box is black");
    // A white edge (in the middle of each side, past the corners).
    assert_eq!(rgb(&switcher, w, x + 1, y + bh / 2), [255, 255, 255], "left edge");
    assert_eq!(rgb(&switcher, w, x + bw - 2, y + bh / 2), [255, 255, 255], "right edge");
    assert_eq!(rgb(&switcher, w, x + bw / 2, y + 1), [255, 255, 255], "top edge");
    // Above the icons and under them (the app's name): only black,
    // white and greys between.
    for (what, strip) in [("above the icons", (x + 16, y + 4, bw - 32, 8)), ("the name", (x + 16, y + bh - 36, bw - 32, 32))] {
        let (other, _) = survey(&switcher, w, strip);
        assert!(other == 0.0, "{what}: {:.1} % another color", other * 100.0);
    }

    // The overview.
    std::thread::sleep(Duration::from_millis(400));
    overview(&session, true);
    assert!(session.wait_for_log("overview drawn", Duration::from_secs(10)), "the overview was not drawn:\n{}", session.log());
    std::thread::sleep(Duration::from_millis(600));
    let (_, _, shown) = session.fresh_snapshot();
    keep(&session, "hc-overview.png");
    overview(&session, false);
    // The search field's white outline crosses the middle column under
    // the panel.
    assert!((32..120).any(|y| rgb(&shown, w, w / 2, y) == [255, 255, 255]), "the search field's white outline");
    // The workspaces under it: the active one outlined in the accent
    // (blue's), black inside, its windows (no pictures in a headless
    // session) outlined in white rather than black on black.
    let accent = |[r, g, b]: [u8; 3]| b > 150 && u16::from(b) > u16::from(r) + 60 && g > r;
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for y in 100..300 {
        for x in 0..w {
            if accent(rgb(&shown, w, x, y)) {
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            }
        }
    }
    assert!(x1 > x0 + 60 && y1 > y0 + 40, "the active workspace's accent outline");
    let inside = (x0 + 4, y0 + 4, x1 - x0 - 8, y1 - y0 - 8);
    let (_, top) = survey(&shown, w, inside);
    assert_eq!(top, [0, 0, 0], "a black workspace");
    let white = (inside.1..inside.1 + inside.3).flat_map(|y| (inside.0..inside.0 + inside.2).map(move |x| (x, y))).filter(|&(x, y)| rgb(&shown, w, x, y).iter().all(|c| *c > 180)).count();
    assert!(white > 20, "its windows are outlined in white ({white} light pixels)");

    for client in &mut clients {
        let _ = client.kill();
        let _ = client.wait();
    }
}
