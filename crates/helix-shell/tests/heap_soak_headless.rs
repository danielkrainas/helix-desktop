//! The shell's heap under long use (helixdesktop/desktop#120): a headless Helix
//! session (helix-comp with helix-shell, scratch home, private bus) with
//! two 1920×1080 monitors and a few client-less notifications has the
//! overview, the launcher (the pointer across its grid), quick settings
//! and the clock's menu opened and closed, and a notification sent, in
//! each of `HELIX_SOAK_ROUNDS` rounds (default 300). The shell logs glibc
//! malloc's counts every 2 s (`HELIX_SHELL_HEAP_LOG`); the shell's
//! anonymous memory (`RssAnon`) and the heap in use after warming up and
//! at the end are compared.
//!
//! Ignored by default (minutes long): `cargo test -p helix-shell --test
//! heap_soak_headless -- --ignored --nocapture`.

use std::path::Path;
use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};

const SIZE: (u32, u32) = (1920, 1080);
const MIB: f64 = 1024.0 * 1024.0;

fn call(session: &HeadlessSession, member: &str, body: impl serde::Serialize + zbus::zvariant::DynamicType) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let conn = session.bus().connect().await;
        let _ = conn.call_method(Some("org.gnome.Shell"), "/org/gnome/Shell", Some("org.freedesktop.DBus.Properties"), member, &body).await;
    });
}

fn overview(session: &HeadlessSession, active: bool) {
    call(session, "Set", ("org.gnome.Shell", "OverviewActive", zbus::zvariant::Value::from(active)));
}

/// Until the shell says the overview (or launcher) is closed.
fn overview_closed(session: &HeadlessSession) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let conn = session.bus().connect().await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let reply = conn.call_method(Some("org.gnome.Shell"), "/org/gnome/Shell", Some("org.freedesktop.DBus.Properties"), "Get", &("org.gnome.Shell", "OverviewActive")).await;
            let open = reply.ok().and_then(|r| r.body().deserialize::<zbus::zvariant::OwnedValue>().ok()).and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
            if !open {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
}

fn show_applications(session: &HeadlessSession) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let conn = session.bus().connect().await;
        let _ = conn.call_method(Some("org.gnome.Shell"), "/org/gnome/Shell", Some("org.gnome.Shell"), "ShowApplications", &()).await;
    });
}

fn notify(session: &HeadlessSession, round: usize) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let conn = session.bus().connect().await;
        let hints: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> = std::collections::HashMap::new();
        let summary = format!("Message {round}");
        let body = format!("Round {round}: the quick brown fox jumps over the lazy dog, again and again.");
        let _ = conn
            .call_method(Some("org.freedesktop.Notifications"), "/org/freedesktop/Notifications", Some("org.freedesktop.Notifications"), "Notify", &("Soak", 0u32, "dialog-information", summary.as_str(), body.as_str(), Vec::<&str>::new(), hints, 1500i32))
            .await;
    });
}

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

/// A `/proc/<pid>/status` field in bytes.
fn status(pid: u32, field: &str) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    status.lines().find_map(|l| l.strip_prefix(field)).and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok()).map_or(0, |kib| kib * 1024)
}

/// The last "shell heap" line's (arena, in_use, free, mmapped).
fn heap(log: &str) -> Option<[u64; 4]> {
    let line = log.lines().rev().find(|l| l.contains("shell heap"))?;
    let field = |name: &str| line.split(&format!("{name}=")).nth(1)?.split_whitespace().next()?.parse::<u64>().ok();
    Some([field("arena")?, field("in_use")?, field("free")?, field("mmapped")?])
}

fn count_opened(log: &str, namespace: &str) -> usize {
    log.matches(&format!("namespace=\"{namespace}\"")).count()
}

#[test]
#[ignore]
fn heap_stays_bounded() {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return;
    };
    let rounds: usize = std::env::var("HELIX_SOAK_ROUNDS").ok().and_then(|s| s.parse().ok()).unwrap_or(300);
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    let settings = "[org/gnome/desktop/interface]\nenable-hot-corners=false\nenable-animations=true\n";
    let mut env = vec![("HELIX_HEADLESS_OUTPUTS".to_owned(), "2".to_owned()), ("HELIX_SHELL_HEAP_LOG".to_owned(), "2".to_owned())];
    // Extra environment for the shell, e.g. MALLOC_ARENA_MAX=…
    if let Ok(extra) = std::env::var("HELIX_SOAK_ENV") {
        env.extend(extra.split_whitespace().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())));
    }
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: Some(&shell), size: SIZE, settings, env }) else { return };
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());
    let deadline = Instant::now() + Duration::from_secs(10);
    let pid = loop {
        if let Some(pid) = shell_pid(session.home()) {
            break pid;
        }
        assert!(Instant::now() < deadline, "no helix-shell process");
        std::thread::sleep(Duration::from_millis(50));
    };
    let (w, h) = (SIZE.0 as i32, SIZE.1 as i32);
    let quick = format!("click {} 12", w - 12);
    let clock = format!("click {} 12", w / 2);
    let wait = |session: &HeadlessSession, namespace: &str, want: usize| {
        let deadline = Instant::now() + Duration::from_secs(3);
        while count_opened(&session.log(), namespace) < want && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let warmup = 30.min(rounds / 3);
    let mut early = None;
    let start = Instant::now();
    for round in 0..rounds {
        overview_closed(&session);
        let log = session.log();
        overview(&session, true);
        wait(&session, "helix-overview", count_opened(&log, "helix-overview") + 2);
        overview(&session, false);
        let log = session.log();
        show_applications(&session);
        wait(&session, "helix-launcher", count_opened(&log, "helix-launcher") + 1);
        let sweep: String = (0..12).map(|i| format!("move {} {}\n", w / 4 + i * w / 24, h / 3 + (i % 4) * 40)).collect();
        session.input(sweep.trim_end());
        let log = session.log();
        session.input(&quick);
        wait(&session, "helix-power-menu", count_opened(&log, "helix-power-menu") + 1);
        session.input(&quick);
        let log = session.log();
        session.input(&clock);
        wait(&session, "helix-notification-list", count_opened(&log, "helix-notification-list") + 1);
        session.input(&clock);
        notify(&session, round);
        if round + 1 == warmup {
            std::thread::sleep(Duration::from_secs(3));
            early = Some((status(pid, "RssAnon:"), heap(&session.log())));
        }
        if (round + 1) % 100 == 0 {
            eprintln!("round {}: RssAnon {:.1} MiB, heap {:?} ({:.0} s)", round + 1, status(pid, "RssAnon:") as f64 / MIB, heap(&session.log()).map(|h| h.map(|v| (v as f64 / MIB * 10.0).round() / 10.0)), start.elapsed().as_secs_f64());
        }
    }
    std::thread::sleep(Duration::from_secs(5));
    let (anon_early, heap_early) = early.expect("warmed up");
    let (anon_end, heap_end) = (status(pid, "RssAnon:"), heap(&session.log()));
    let threads = std::fs::read_dir(format!("/proc/{pid}/task")).map(|d| d.count()).unwrap_or(0);
    eprintln!(
        "{rounds} rounds in {:.0} s: RssAnon {:.1} → {:.1} MiB; heap (arena, in use, free, mmapped) {:?} → {:?} MiB; {threads} threads",
        start.elapsed().as_secs_f64(),
        anon_early as f64 / MIB,
        anon_end as f64 / MIB,
        heap_early.map(|h| h.map(|v| v / (1 << 20))),
        heap_end.map(|h| h.map(|v| v / (1 << 20))),
    );
    // The bound: what is in use doesn't grow by more than 16 MiB over the
    // run, and anonymous memory by more than 32 MiB.
    if let (Some(a), Some(b)) = (heap_early, heap_end) {
        assert!(b[1] <= a[1] + 16 * (1 << 20), "heap in use grew from {} to {} MiB", a[1] >> 20, b[1] >> 20);
    }
    assert!(anon_end <= anon_early + 32 * (1 << 20), "anonymous memory grew from {} to {} MiB", anon_early >> 20, anon_end >> 20);
}
