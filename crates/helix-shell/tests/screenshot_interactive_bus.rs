//! `InteractiveScreenshot` saves in the Screenshots folder (its own test
//! binary: it points HOME at a scratch directory before GLib reads it).

use std::sync::{Arc, Mutex};

use helix_shell::screenshot::{Job, Picked, Rect};
use helix_shell::{ShellService, UiEvent};
use helix_test_support::TestBus;

const PATH: &str = "/org/gnome/Shell/Screenshot";
const IFACE: &str = "org.gnome.Shell.Screenshot";

/// Answers picks in order; captures write a few bytes.
fn fake_ui(mut ui: tokio::sync::mpsc::UnboundedReceiver<UiEvent>, picks: Arc<Mutex<Vec<Picked>>>) {
    tokio::spawn(async move {
        while let Some(event) = ui.recv().await {
            let UiEvent::Screenshot(slot) = event else { continue };
            match slot.take() {
                Some(Job::Capture { mut file, reply, .. }) => {
                    std::io::Write::write_all(&mut file, b"\x89PNG").unwrap();
                    let _ = reply.send(Ok(Rect::new(0, 0, 10, 10)));
                }
                Some(Job::Pick { reply, .. }) => {
                    let mut picks = picks.lock().unwrap();
                    let answer = if picks.is_empty() { Picked::Cancelled } else { picks.remove(0) };
                    let _ = reply.send(answer);
                }
                _ => {}
            }
        }
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn interactive_screenshot_is_saved_in_screenshots() {
    let home = std::env::temp_dir().join(format!("helix-interactive-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    // Before any other thread or GLib reads the environment.
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", home.join("config"));
    let Some(bus) = TestBus::start() else { return };
    let (_shell, ui) = ShellService::start(bus.builder()).await.unwrap();
    fake_ui(ui, Arc::new(Mutex::new(vec![Picked::Cancelled, Picked::Screen])));
    let portal = bus.connect().await;
    portal.request_name("org.freedesktop.impl.portal.desktop.helix").await.unwrap();

    let call = async || -> (bool, String) {
        let reply = portal.call_method(Some("org.gnome.Shell"), PATH, Some(IFACE), "InteractiveScreenshot", &()).await.unwrap();
        reply.body().deserialize().unwrap()
    };
    let (ok, uri) = call().await;
    assert_eq!((ok, uri.as_str()), (false, ""), "cancelled");
    let (ok, uri) = call().await;
    assert!(ok);
    assert!(uri.starts_with("file://") && uri.contains("/Screenshots/Screenshot%20From%20"), "{uri}");
    let path = gio_path(&uri);
    assert!(path.starts_with(home.join("Screenshots")), "{}", path.display());
    assert!(path.exists());
    std::fs::remove_dir_all(&home).unwrap();
}

fn gio_path(uri: &str) -> std::path::PathBuf {
    let raw = uri.strip_prefix("file://").unwrap();
    std::path::PathBuf::from(raw.replace("%20", " "))
}
