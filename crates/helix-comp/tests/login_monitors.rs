//! `helix-login-monitors` as pkexec runs it, in a scratch root
//! (`HELIX_LOGIN_MONITORS_TEST_ROOT`, honoured only when not root), and
//! its polkit policy (helix-desktop-spec `components/greeter.md`, "The
//! monitor").

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const SET: &str = "DP-3|DEL|U2720Q|ABC123;DVI-D-1|SAM|S24|0";

fn input() -> String {
    format!(
        "[[configuration]]\nmonitors = \"{SET}\"\n\n\
         [[configuration.logical]]\nx = 0\ny = 0\nscale = 1.0\ntransform = 0\nprimary = true\n\
         [[configuration.logical.monitors]]\nconnector = \"DP-3\"\nwidth = 2560\nheight = 1440\nrefresh_mhz = 60000\n\n\
         [[configuration.logical]]\nx = 2560\ny = 0\nscale = 1.0\ntransform = 0\nprimary = false\n\
         [[configuration.logical.monitors]]\nconnector = \"DVI-D-1\"\nwidth = 1920\nheight = 1080\nrefresh_mhz = 60000\n"
    )
}

/// Run the helper with `stdin` and `args`; its exit status.
fn run(root: &Path, args: &[&str], stdin: &str) -> i32 {
    let mut child = Command::new(env!("CARGO_BIN_EXE_helix-login-monitors"))
        .args(args)
        .env_clear()
        .env("HELIX_LOGIN_MONITORS_TEST_ROOT", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // It may exit before reading (usage).
    let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
    child.wait().unwrap().code().unwrap_or(-1)
}

#[test]
fn helper_writes_into_the_scratch_root() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("running as root: the test root would be ignored; skipped");
        return;
    }
    let root = std::env::temp_dir().join(format!("helix-login-monitors-bin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let target: PathBuf = root.join("etc/xdg/helix/monitors.toml");

    assert_eq!(run(&root, &["--root", "/"], &input()), 64, "no arguments");
    assert_eq!(run(&root, &[], "monitors = \"x\""), 1, "invalid input");
    assert!(!target.exists(), "nothing written");

    assert_eq!(run(&root, &[], &input()), 0);
    let meta = std::fs::metadata(&target).unwrap();
    assert_eq!(std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o7777, 0o644);
    let store = helix_comp::display_config::Store::at(target.clone());
    let config = store.load(SET).expect("the configuration, for the compositor");
    assert!(config.logical[0].primary);
    assert_eq!(config.logical[0].monitors[0].connector, "DP-3");
    let _ = std::fs::remove_dir_all(&root);
}

/// Like `pkaction --verbose` would show it, without installing it.
#[test]
fn polkit_policy() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/org.helix.login-screen-monitors.policy");
    let text = std::fs::read_to_string(&path).unwrap();
    let doc = roxmltree::Document::parse_with_options(&text, roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() }).expect("well-formed XML");
    let root = doc.root_element();
    assert_eq!(root.tag_name().name(), "policyconfig");
    let actions: Vec<_> = root.children().filter(|n| n.has_tag_name("action")).collect();
    assert_eq!(actions.len(), 1);
    let action = actions[0];
    assert_eq!(action.attribute("id"), Some("org.helix.login-screen-monitors"));
    let child = |node: roxmltree::Node<'_, '_>, name: &str| node.children().find(|n| n.has_tag_name(name)).and_then(|n| n.text()).map(str::to_owned);
    assert!(child(action, "description").is_some_and(|d| !d.is_empty()));
    assert!(child(action, "message").is_some_and(|m| !m.is_empty()));
    let defaults = action.children().find(|n| n.has_tag_name("defaults")).expect("defaults");
    assert_eq!(child(defaults, "allow_any").as_deref(), Some("auth_admin"));
    assert_eq!(child(defaults, "allow_inactive").as_deref(), Some("auth_admin"));
    assert_eq!(child(defaults, "allow_active").as_deref(), Some("auth_admin_keep"));
    let exec = action
        .children()
        .find(|n| n.has_tag_name("annotate") && n.attribute("key") == Some("org.freedesktop.policykit.exec.path"))
        .and_then(|n| n.text());
    assert_eq!(exec, Some("/usr/libexec/helix/helix-login-monitors"));

    // Packaged where pkexec looks, from where the build puts it.
    let stage = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/stage.sh")).unwrap();
    assert!(stage.contains("/usr/share/polkit-1/actions/org.helix.login-screen-monitors.policy"));
    assert!(stage.contains("/usr/libexec/helix/helix-login-monitors"));
    let nfpm = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/nfpm.yaml")).unwrap();
    assert!(nfpm.contains("dst: /usr/share/polkit-1/actions/org.helix.login-screen-monitors.policy"));
    assert!(nfpm.contains("dst: /usr/libexec/helix/helix-login-monitors"));
}
