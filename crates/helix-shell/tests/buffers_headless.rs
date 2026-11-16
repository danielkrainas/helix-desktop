//! The shell's shared-memory buffers stay bounded (helixdesktop/desktop#100): a
//! headless Helix session (helix-comp with helix-shell, scratch home,
//! private bus) with two 1920×1080 monitors has the overview, the
//! launcher, quick settings and the clock's menu opened and closed 200
//! times, with the pointer sweeping the app grid (hover fades) each time.
//!
//! - The shell's memory files (its buffers; read from /proc/<pid>/fd)
//!   never add up to 100 MiB, and once everything is closed they shrink
//!   back to what the panel and backgrounds need.
//! - Its resident memory does not climb from round to round.
//! - No surface ever had more than three buffers with the compositor
//!   (the shell logs the most it had: "shell buffers … peak_busy=").
//!
//! Needs helix-comp built next to this test; skipped otherwise.

use std::path::Path;
use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};

const SIZE: (u32, u32) = (1920, 1080);
const ROUNDS: usize = 200;
const MIB: u64 = 1024 * 1024;

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

/// Show the launcher (`ShowApplications`).
fn show_applications(session: &HeadlessSession) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let conn = session.bus().connect().await;
        conn.call_method(Some("org.gnome.Shell"), "/org/gnome/Shell", Some("org.gnome.Shell"), "ShowApplications", &()).await.expect("ShowApplications");
    });
}

/// The session's helix-shell process (the one whose HOME is the
/// session's scratch home).
fn shell_pid(home: &Path) -> Option<u32> {
    let want = format!("HOME={}", home.display());
    std::fs::read_dir("/proc").ok()?.flatten().find_map(|entry| {
        let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
        let comm = std::fs::read_to_string(entry.path().join("comm")).ok()?;
        if comm.trim() != "helix-shell" {
            return None;
        }
        let environ = std::fs::read(entry.path().join("environ")).ok()?;
        environ.split(|b| *b == 0).any(|v| v == want.as_bytes()).then_some(pid)
    })
}

/// The total size of the process's memory files (its wl_shm pools).
fn memfd_bytes(pid: u32) -> u64 {
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { return 0 };
    fds.flatten()
        .filter(|fd| std::fs::read_link(fd.path()).is_ok_and(|l| l.to_string_lossy().contains("memfd:")))
        .filter_map(|fd| std::fs::metadata(fd.path()).ok())
        .map(|m| m.len())
        .sum()
}

/// Resident memory (bytes).
fn rss(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .map_or(0, |kib| kib * 1024)
}

/// Until the shell has told the bus the overview (or launcher) is closed.
fn overview_closed(session: &HeadlessSession) {
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
            assert!(Instant::now() < deadline, "the overview stayed open");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
}

/// How many times the compositor saw a `namespace` layer surface made.
fn count_opened(log: &str, namespace: &str) -> usize {
    log.matches(&format!("namespace=\"{namespace}\"")).count()
}

/// The most buffers one surface had with the compositor, as logged.
fn peak_busy(log: &str) -> usize {
    log.lines().filter(|l| l.contains("shell buffers")).filter_map(|l| l.split("peak_busy=").nth(1)?.split_whitespace().next()?.parse().ok()).max().unwrap_or(0)
}

#[test]
fn buffers_stay_bounded() {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return;
    };
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    let settings = "[org/gnome/desktop/interface]\nenable-hot-corners=false\nenable-animations=true\n";
    let env = vec![
        ("RUST_LOG".to_owned(), "info,helix_shell::ui::buffers=debug".to_owned()),
        ("HELIX_HEADLESS_OUTPUTS".to_owned(), "2".to_owned()),
    ];
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: Some(&shell), size: SIZE, settings, env }) else { return };
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());
    let deadline = Instant::now() + Duration::from_secs(10);
    let pid = loop {
        if let Some(pid) = shell_pid(session.home()) {
            break pid;
        }
        assert!(Instant::now() < deadline, "no helix-shell process for the session");
        std::thread::sleep(Duration::from_millis(50));
    };
    std::thread::sleep(Duration::from_secs(1));
    let (w, h) = (SIZE.0 as i32, SIZE.1 as i32);
    let pause = || std::thread::sleep(Duration::from_millis(15));
    let mut opened: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    // Wait until `namespace` was opened once more; with `click`, click
    // (again, if a surface closing took the click) until it is.
    let mut wait_opened = |session: &mut HeadlessSession, namespace: &'static str, more: usize, click: Option<&str>| {
        let want = *opened.entry(namespace).or_insert_with(|| count_opened(&session.log(), namespace)) + more;
        for _ in 0..3 {
            if let Some(click) = click {
                session.input(click);
            }
            let deadline = Instant::now() + Duration::from_secs(if click.is_some() { 2 } else { 10 });
            while count_opened(&session.log(), namespace) < want && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if count_opened(&session.log(), namespace) >= want {
                opened.insert(namespace, want);
                return;
            }
        }
        panic!("{namespace} did not open:\n{}", session.log());
    };
    let quick = format!("click {} 12", w - 12);
    let clock = format!("click {} 12", w / 2);
    let mut most = memfd_bytes(pid);
    let mut rss_early = 0;
    for round in 0..ROUNDS {
        // The overview, on both monitors.
        overview_closed(&session);
        overview(&session, true);
        wait_opened(&mut session, "helix-overview", 2, None);
        pause();
        most = most.max(memfd_bytes(pid));
        overview(&session, false);
        // The launcher, the pointer across its grid and back (each cell
        // lit and faded).
        show_applications(&session);
        wait_opened(&mut session, "helix-launcher", 1, None);
        pause();
        let sweep: String = (0..16).map(|i| format!("move {} {}\n", w / 4 + i * w / 32, h / 3 + (i % 4) * 40)).collect();
        session.input(sweep.trim_end());
        most = most.max(memfd_bytes(pid));
        // Quick settings (the panel's right end; the launcher closes),
        // closed by another click there (on the panel, or beside the
        // menu).
        wait_opened(&mut session, "helix-power-menu", 1, Some(&quick));
        pause();
        most = most.max(memfd_bytes(pid));
        session.input(&quick);
        // The clock's menu (notifications and calendar), the same way.
        wait_opened(&mut session, "helix-notification-list", 1, Some(&clock));
        pause();
        most = most.max(memfd_bytes(pid));
        session.input(&clock);
        if round == 19 {
            rss_early = rss(pid);
        }
    }
    // Everything closed; buffers not painted again for a while are freed.
    std::thread::sleep(Duration::from_secs(7));
    let settled = memfd_bytes(pid);
    let rss_end = rss(pid);
    let log = session.log();
    let peak = peak_busy(&log);
    eprintln!(
        "shm pools: most {} MiB, settled {} MiB; RSS after 20 rounds {} MiB, at the end {} MiB; most buffers out per surface {peak}",
        most / MIB,
        settled / MIB,
        rss_early / MIB,
        rss_end / MIB
    );
    assert!(most < 100 * MIB, "the shell's buffers reached {} MiB", most / MIB);
    // Two backgrounds (8 MiB each) and the panel, with room to spare.
    assert!(settled < 40 * MIB, "closed surfaces kept their buffers: {} MiB", settled / MIB);
    assert!(rss_end < rss_early + 40 * MIB, "RSS climbed from {} MiB to {} MiB", rss_early / MIB, rss_end / MIB);
    assert!((1..=3).contains(&peak), "a surface had {peak} buffers with the compositor");
}
