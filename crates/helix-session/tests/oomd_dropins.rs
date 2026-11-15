//! The systemd-oomd drop-ins the packages install (session/systemd): their
//! settings, that packaging installs one per listed unit, and, where
//! systemd-analyze is available, that systemd parses them without
//! complaint. Nothing is installed into the running user manager.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &str) -> String {
    std::fs::read_to_string(repo().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// `key=value` pairs of `section` in a unit file, comments skipped.
fn section(text: &str, wanted: &str) -> Vec<(String, String)> {
    let mut current = String::new();
    let mut pairs = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = name.to_string();
        } else if current == wanted {
            let (key, value) = line.split_once('=').expect("key=value");
            pairs.push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    pairs
}

fn units() -> Vec<String> {
    read("session/systemd/oomd-avoid.units")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

const DROP_IN: &str = "50-helix-oomd-avoid.conf";

#[test]
fn drop_in_avoids_oomd() {
    let conf = read("session/systemd/helix-oomd-avoid.conf");
    let service = section(&conf, "Service");
    assert!(service.contains(&("ManagedOOMPreference".into(), "avoid".into())), "{service:?}");
    assert!(service.contains(&("OOMScoreAdjust".into(), "100".into())), "{service:?}");
    // Nothing else: the drop-in must not change how the units run.
    assert_eq!(service.len(), 2, "{service:?}");
}

#[test]
fn units_cover_the_session_core() {
    let units = units();
    for core in ["dbus.service", "dbus-broker.service", "at-spi-dbus-bus.service", "xdg-desktop-portal.service"] {
        assert!(units.iter().any(|u| u == core), "{core} must be spared");
    }
    assert!(units.iter().all(|u| u.ends_with(".service")), "{units:?}");
}

#[test]
fn packaging_installs_every_drop_in() {
    let stage = read("packaging/stage.sh");
    assert!(stage.contains("session/systemd/oomd-avoid.units"), "stage.sh reads the unit list");
    assert!(stage.contains(&format!("$unit.d/{DROP_IN}")), "stage.sh installs per unit");
    let nfpm = read("packaging/nfpm.yaml");
    for unit in units() {
        let path = format!("/usr/lib/systemd/user/{unit}.d/{DROP_IN}");
        assert!(nfpm.contains(&format!("src: ./stage{path}")), "nfpm.yaml packages {path}");
        assert!(nfpm.contains(&format!("dst: {path}")), "nfpm.yaml installs {path}");
    }
}

/// systemd-analyze verifies a stand-in unit with the drop-in from a scratch
/// unit directory (searched before the installed units, which it only
/// reads).
#[test]
fn systemd_parses_the_drop_in() {
    let dir = std::env::temp_dir().join(format!("helix-oomd-verify-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dbus.service.d")).unwrap();
    std::fs::write(dir.join("dbus.service"), "[Service]\nExecStart=/bin/true\n").unwrap();
    std::fs::copy(repo().join("session/systemd/helix-oomd-avoid.conf"), dir.join("dbus.service.d").join(DROP_IN))
        .unwrap();
    let output = Command::new("systemd-analyze")
        .args(["--user", "verify", "--man=no", "--generators=no"])
        .arg(dir.join("dbus.service"))
        // The trailing colon keeps the default search path after ours.
        .env("SYSTEMD_UNIT_PATH", format!("{}:", dir.display()))
        .output();
    let _ = std::fs::remove_dir_all(&dir);
    let Ok(output) = output else {
        eprintln!("systemd-analyze not available; skipped");
        return;
    };
    let messages = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    let complaints: Vec<&str> = messages.lines().filter(|l| l.contains(DROP_IN)).collect();
    assert!(complaints.is_empty(), "systemd complains about the drop-in: {complaints:?}");
}
