//! Enter answers a prompt dialog (helixdesktop/desktop#71: the run dialog, like
//! every prompt, never took Enter: the time it was first shown was not
//! noted for prompts, so Enter always came "too soon"). A headless Helix
//! session (helix-comp with helix-shell, scratch home, private bus):
//! Alt+F2, a command typed, Enter, and the command runs.
//!
//! Needs helix-comp built next to this test; skipped otherwise.

use std::time::Duration;

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};

#[test]
fn enter_runs_the_command() {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return;
    };
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    let settings = "[org/gnome/desktop/interface]\nenable-hot-corners=false\n";
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: Some(&shell), size: (1280, 720), settings, env: Vec::new() }) else { return };
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());
    // Alt+F2, then the command once the dialog is up.
    session.input("tap 56 60");
    assert!(session.wait_for_log("namespace=\"helix-dialog\"", Duration::from_secs(10)), "the run dialog:\n{}", session.log());
    std::thread::sleep(Duration::from_millis(800));
    session.input("type true");
    std::thread::sleep(Duration::from_millis(500));
    session.input("tap 28");
    assert!(session.wait_for_log("run dialog command=\"true\"", Duration::from_secs(10)), "Enter ran the command:\n{}", session.log());
}

/// What the shell starts gets an activation token of its own
/// (helixdesktop/desktop#119), which helix-comp grants: the command sees
/// it as `XDG_ACTIVATION_TOKEN`.
#[test]
fn launches_carry_the_shells_token() {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return;
    };
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    let settings = "[org/gnome/desktop/interface]\nenable-hot-corners=false\n";
    let env = vec![("RUST_LOG".to_owned(), "info,helix_comp=debug".to_owned())];
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: Some(&shell), size: (1280, 720), settings, env }) else { return };
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());
    session.input("tap 56 60");
    assert!(session.wait_for_log("namespace=\"helix-dialog\"", Duration::from_secs(10)), "the run dialog:\n{}", session.log());
    std::thread::sleep(Duration::from_millis(800));
    session.input("type sh -c 'echo \"$XDG_ACTIVATION_TOKEN\" >\"$HOME/token\"'");
    std::thread::sleep(Duration::from_millis(500));
    session.input("tap 28");
    let file = session.home().join("token");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let token = loop {
        let token = std::fs::read_to_string(&file).unwrap_or_default();
        if !token.trim().is_empty() || std::time::Instant::now() > deadline {
            break token;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(!token.trim().is_empty(), "the command got no activation token:\n{}", session.log());
    assert!(session.log().contains("activation token ok=true privileged=true"), "the shell's token is granted:\n{}", session.log());
}
