//! `helix-login-config` as pkexec runs it, and its polkit policy and
//! packaging (helixdesktop/spec `components/greeter.md`, "Changing the
//! configuration").
//!
//! The binary always writes `/etc/helix/greeter.toml`, so it is only run
//! here with input it refuses before opening anything; writing is tested
//! in `helix_greeter::login_config` on a scratch directory.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Run the helper with `args` and `stdin`; its exit status and standard
/// error.
fn run(args: &[&str], stdin: &[u8]) -> (i32, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_helix-login-config"))
        .args(args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // It may exit before reading (arguments, too much input).
    let _ = child.stdin.take().unwrap().write_all(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(out.stdout.is_empty(), "prints nothing on standard output");
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn refuses_arguments_and_invalid_input() {
    let scratch = std::env::temp_dir().join(format!("helix-login-config-bin-{}", std::process::id()));
    let target = scratch.join("greeter.toml");
    let target = target.to_str().unwrap();
    assert_eq!(run(&[target], b"user-list = false").0, 64, "a path");
    assert_eq!(run(&["--help"], b"").0, 64);
    assert!(!scratch.exists());

    let (status, err) = run(&[], b"accent = \"magenta\"");
    assert_eq!(status, 1);
    assert!(err.starts_with("helix-login-config: invalid input: accent must be one of") && err.trim_end().lines().count() == 1, "{err}");
    assert_eq!(run(&[], b"theme = \"dark\"").0, 1);
    assert_eq!(run(&[], b"banner = \"a\\u202Eb\"").0, 1);
    assert_eq!(run(&[], b"user-list = false\nremove = [\"user-list\"]").0, 1);
    assert_eq!(run(&[], &[0xff, 0xfe]).0, 1);
    let mut big = b"banner = \"x\"\n".to_vec();
    big.resize(64 * 1024, b'#');
    assert_eq!(run(&[], &big).0, 1, "more than 16 KiB");
}

/// Like `pkaction --verbose` would show it, without installing it.
#[test]
fn polkit_policy() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let text = std::fs::read_to_string(root.join("data/org.helixdesktop.login-screen-config.policy")).unwrap();
    let doc = roxmltree::Document::parse_with_options(&text, roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() }).expect("well-formed XML");
    let policy = doc.root_element();
    assert_eq!(policy.tag_name().name(), "policyconfig");
    let actions: Vec<_> = policy.children().filter(|n| n.has_tag_name("action")).collect();
    assert_eq!(actions.len(), 1);
    let action = actions[0];
    assert_eq!(action.attribute("id"), Some("org.helixdesktop.login-screen-config"));
    let child = |node: roxmltree::Node<'_, '_>, name: &str| node.children().find(|n| n.has_tag_name(name)).and_then(|n| n.text()).map(str::to_owned);
    assert!(child(action, "description").is_some_and(|d| !d.is_empty()));
    assert_eq!(child(action, "message").as_deref(), Some("Authentication is required to change the login screen settings"));
    let defaults = action.children().find(|n| n.has_tag_name("defaults")).expect("defaults");
    assert_eq!(child(defaults, "allow_any").as_deref(), Some("no"));
    assert_eq!(child(defaults, "allow_inactive").as_deref(), Some("no"));
    assert_eq!(child(defaults, "allow_active").as_deref(), Some("auth_admin_keep"));
    let exec = action
        .children()
        .find(|n| n.has_tag_name("annotate") && n.attribute("key") == Some("org.freedesktop.policykit.exec.path"))
        .and_then(|n| n.text());
    assert_eq!(exec, Some("/usr/libexec/helix/helix-login-config"));

    // Staged and packaged in helix-greeter, where pkexec looks.
    let stage = std::fs::read_to_string(root.join("packaging/stage.sh")).unwrap();
    assert!(stage.contains("/usr/share/polkit-1/actions/org.helixdesktop.login-screen-config.policy"));
    assert!(stage.contains("/usr/libexec/helix/helix-login-config"));
    let nfpm = std::fs::read_to_string(root.join("packaging/nfpm-greeter.yaml")).unwrap();
    assert!(nfpm.contains("dst: /usr/share/polkit-1/actions/org.helixdesktop.login-screen-config.policy"));
    assert!(nfpm.contains("dst: /usr/libexec/helix/helix-login-config"));
}
