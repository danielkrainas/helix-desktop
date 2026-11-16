//! The login screen goes on the primary monitor of the system-wide layout
//! (helixdesktop/spec `components/greeter.md`, "The monitor"): a headless
//! two-monitor helix-comp, with no layout of the greeter user's own and a
//! system one (`$XDG_CONFIG_DIRS/helix/monitors.toml`) making the second
//! monitor primary, runs the greeter (`--demo`) as its shell.
//!
//! Needs helix-comp built next to this test; skipped otherwise.

use std::time::Duration;

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};

const SYSTEM_LAYOUT: &str = r#"[[configuration]]
monitors = "HEADLESS-1|Helix|Headless|0;HEADLESS-2|Helix|Headless|1"

[[configuration.logical]]
x = 0
y = 0
scale = 1.0
transform = 0
primary = true

[[configuration.logical.monitors]]
connector = "HEADLESS-2"
width = 800
height = 600
refresh_mhz = 60000

[[configuration.logical]]
x = 800
y = 0
scale = 1.0
transform = 0
primary = false

[[configuration.logical.monitors]]
connector = "HEADLESS-1"
width = 800
height = 600
refresh_mhz = 60000
"#;

#[test]
fn login_screen_on_the_system_layouts_primary() {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return;
    };
    let scratch = std::env::temp_dir().join(format!("helix-greeter-primary-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let system = scratch.join("xdg");
    std::fs::create_dir_all(system.join("helix")).unwrap();
    let system_file = system.join("helix/monitors.toml");
    std::fs::write(&system_file, SYSTEM_LAYOUT).unwrap();
    // The greeter in demo mode (no greetd), as the compositor's shell.
    let shell = scratch.join("greeter.sh");
    std::fs::write(&shell, format!("#!/bin/sh\nexec '{}' --demo\n", env!("CARGO_BIN_EXE_helix-greeter"))).unwrap();
    std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let Some(session) = HeadlessSession::start(Options {
        comp: &comp,
        shell: Some(&shell),
        size: (800, 600),
        settings: "",
        env: vec![
            ("HELIX_HEADLESS_OUTPUTS".into(), "2".into()),
            ("XDG_CONFIG_DIRS".into(), system.display().to_string()),
            ("HELIX_NO_ATSPI".into(), "1".into()),
        ],
    }) else {
        let _ = std::fs::remove_dir_all(&scratch);
        return;
    };
    let shown = session.wait_for_log(r#"the login screen's monitor primary=Some("HEADLESS-2")"#, Duration::from_secs(30));
    let log = session.log();
    assert!(shown, "the login screen did not go to HEADLESS-2:\n{log}");
    assert!(log.contains("using the system-wide monitor configuration"), "{log}");
    let last = log.lines().rev().find(|l| l.contains("the login screen's monitor")).unwrap_or_default();
    assert!(last.contains(r#"Some("HEADLESS-2")"#), "it stays there: {last}");
    assert_eq!(std::fs::read_to_string(&system_file).unwrap(), SYSTEM_LAYOUT, "the system file is only read");
    assert!(!session.home().join(".config/helix/monitors.toml").exists(), "nothing remembered for the greeter user");
    drop(session);
    let _ = std::fs::remove_dir_all(&scratch);
}
