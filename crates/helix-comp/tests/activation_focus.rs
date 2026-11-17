//! Focus-stealing prevention (helixdesktop/spec `contracts/wayland/README.md`,
//! "Windows wanting attention"; helixdesktop/desktop#119): an
//! `xdg_activation_v1` token takes focus only when it carries the serial
//! of the seat's latest key or button press, asked for by the client that
//! press went to (or when the shell made it). Other tokens (no serial,
//! serial 0, a stale serial, another client's press) make the window want
//! attention and leave focus where it is.
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use helix_test_support::headless::{HeadlessSession, Options};

#[test]
fn tokens_need_the_users_input() {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let env = vec![("RUST_LOG".to_owned(), "info,helix_comp=debug".to_owned())];
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: None, size: (1280, 800), settings: "", env }) else { return };
    // The thief, with two windows, first; then the window the user works
    // in. Wide and tall: whatever is on top, some of each is uncovered.
    let mut thief = common::connect(&session);
    let (b1, ()) = thief.toplevel((900, 150), |_, _| {});
    let (b2, ()) = thief.toplevel((900, 150), |_, _| {});
    let mut user = common::connect(&session);
    let (a, ()) = user.toplevel((300, 600), |_, _| {});
    let a_surface = a.surface.clone();
    user.until("the new window has no focus", |s| s.keyboard.as_ref() == Some(&a_surface));
    let a_focused = |user: &mut common::Client| user.focused().as_ref() == Some(&a_surface);

    // The user types into a; the thief mints tokens of its own.
    session.input("tap 30");
    user.roundtrip();
    assert!(user.state.press.is_some(), "the key press did not reach the focused client");
    for (what, from, serial) in [("no surface, no serial", None, None), ("serial 0, no surface", None, Some(0)), ("its own surface, no serial", Some(&b1.surface), None)] {
        let token = thief.token(from, serial);
        thief.activate_token(token, &b1.surface);
        assert!(a_focused(&mut user), "a token with {what} took focus");
    }
    // The serial of the user's key press, but asked for by the thief.
    let pressed = user.state.press;
    let token = thief.token(Some(&b1.surface), pressed);
    thief.activate_token(token, &b1.surface);
    assert!(a_focused(&mut user), "another client's key press let the thief take focus");
    assert!(session.log().contains("granted=false"), "denied activations are logged");

    // A click on b2 (the thief's last window, on top of b1): the
    // thief's own click serial lets it activate b1.
    let (x, y) = common::point_over(&mut session, &mut thief, &b2.surface, (1280, 800));
    session.input(&format!("click {x} {y}"));
    thief.roundtrip();
    let click = thief.state.press;
    assert!(click.is_some(), "the click did not reach b2");
    thief.activate(&b1.surface);
    assert_eq!(thief.focused().as_ref(), Some(&b1.surface), "a token for the user's click on b2 did not activate b1");

    // A key press in b1: a token for it activates b2.
    session.input("tap 30");
    thief.activate(&b2.surface);
    assert_eq!(thief.focused().as_ref(), Some(&b2.surface), "a token for the user's key press did not activate b2");

    // The user clicks back into a: the thief's older serial is stale now.
    let (x, y) = common::point_over(&mut session, &mut user, &a_surface, (1280, 800));
    session.input(&format!("click {x} {y}"));
    assert!(a_focused(&mut user), "the click did not focus a");
    let token = thief.token(Some(&b2.surface), click);
    thief.activate_token(token, &b1.surface);
    assert!(a_focused(&mut user), "a stale serial took focus");

    let log = session.log();
    assert!(!log.contains("panicked"), "{log}");
}
