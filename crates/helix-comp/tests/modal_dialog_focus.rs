//! Focus meant for a window goes to its modal dialog (helixdesktop/spec
//! `contracts/wayland/README.md`, "Window-management settings", Focus),
//! with the dialog made modal the way GTK 3 does it
//! (`gtk_surface1.set_modal`). This is what Electron apps such as VS Code
//! do with a "Save changes?" dialog: they activate their main window
//! (`xdg_activation_v1`, with a fresh token) right after opening the
//! dialog, and give the dialog its parent only after that. Before the
//! fix the main window kept focus and the dialog took no keys.
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use helix_test_support::headless::{HeadlessSession, Options};

#[test]
fn focus_goes_to_the_modal_dialog() {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: None, size: (1280, 800), settings: "", env: Vec::new() }) else { return };
    let mut c = common::connect(&session);
    let (main, ()) = c.toplevel((800, 600), |_, _| {});
    let main_surface = main.surface.clone();
    c.until("the new main window has no focus", |s| s.keyboard.as_ref() == Some(&main_surface));

    // A modal dialog of it, made modal as GTK 3 does.
    let (dialog, gtk) = c.toplevel((200, 100), |c, t| {
        let gtk = c.gtk_shell.get_gtk_surface(&t.surface, &c.qh, ());
        gtk.set_modal();
        t.toplevel.set_parent(Some(&main.toplevel));
        gtk
    });
    let dialog_surface = dialog.surface.clone();
    c.until("the new dialog has no focus", |s| s.keyboard.as_ref() == Some(&dialog_surface));

    // After a key press the app activates its main window (with that
    // press's serial, so the token is granted): the dialog keeps focus.
    session.input("tap 30");
    c.activate(&main.surface);
    assert_eq!(c.focused().as_ref(), Some(&dialog.surface), "activating the main window took focus from its modal dialog");

    // A click on the main window (outside the dialog): still the dialog.
    let (x, y) = common::point_over(&mut session, &mut c, &main_surface, (1280, 800));
    session.input(&format!("click {x} {y}"));
    assert_eq!(c.focused().as_ref(), Some(&dialog.surface), "a click on the main window took focus from its modal dialog");

    // The parent comes after the activation (VS Code's order): when the
    // dialog gets its parent, focus moves on to it.
    dialog.toplevel.set_parent(None);
    c.roundtrip();
    session.input("tap 30");
    c.activate(&main.surface);
    assert_eq!(c.focused().as_ref(), Some(&main.surface), "an activation for the latest key press was not granted");
    dialog.toplevel.set_parent(Some(&main.toplevel));
    c.until("the dialog did not get focus when it got its parent", |s| s.keyboard.as_ref() == Some(&dialog_surface));

    // A dialog that is not modal does not keep focus from its parent.
    gtk.unset_modal();
    c.roundtrip();
    session.input("tap 30");
    c.activate(&main.surface);
    assert_eq!(c.focused().as_ref(), Some(&main.surface), "a dialog that is not modal kept focus from its parent");
    // Made modal again while its parent has focus: focus moves to it.
    gtk.set_modal();
    c.until("the dialog did not get focus when made modal", |s| s.keyboard.as_ref() == Some(&dialog_surface));

    let log = session.log();
    assert!(!log.contains("panicked"), "{log}");
}
