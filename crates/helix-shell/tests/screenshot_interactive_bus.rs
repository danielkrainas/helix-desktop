//! `InteractiveScreenshot` saves in the Screenshots folder (its own test
//! binary: it points HOME at a scratch directory before GLib reads it).

use std::sync::{Arc, Mutex};

use helix_shell::screenshot::{Job, Picked, Rect};
use helix_shell::{ShellService, UiEvent, UiRequest};

struct FileManager {
    shown: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.freedesktop.FileManager1")]
impl FileManager {
    async fn show_items(&self, uris: Vec<String>, _startup_id: &str) {
        self.shown.lock().unwrap().extend(uris);
    }
}
use helix_test_support::TestBus;

const PATH: &str = "/org/gnome/Shell/Screenshot";
const IFACE: &str = "org.gnome.Shell.Screenshot";

/// (id, summary, buttons)
type Banner = (u32, String, Vec<(String, String)>);

/// What the stand-in UI saw besides picks and captures.
#[derive(Default)]
struct Seen {
    copied: Vec<Vec<u8>>,
    banners: Vec<Banner>,
}

/// Answers picks in order; captures write a few bytes.
fn fake_ui(mut ui: tokio::sync::mpsc::UnboundedReceiver<UiEvent>, picks: Arc<Mutex<Vec<Picked>>>, seen: Arc<Mutex<Seen>>) {
    tokio::spawn(async move {
        while let Some(event) = ui.recv().await {
            if let UiEvent::Banner(Some(b)) = &event {
                seen.lock().unwrap().banners.push((b.id, b.summary.clone(), b.buttons.clone()));
                continue;
            }
            let UiEvent::Screenshot(slot) = event else { continue };
            match slot.take() {
                Some(Job::Copy(mut file)) => {
                    let mut bytes = Vec::new();
                    std::io::Read::read_to_end(&mut file, &mut bytes).unwrap();
                    seen.lock().unwrap().copied.push(bytes);
                }
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
    let (shell, ui) = ShellService::start(bus.builder()).await.unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    fake_ui(ui, Arc::new(Mutex::new(vec![Picked::Cancelled, Picked::Screen])), seen.clone());
    let files = Arc::new(Mutex::new(Vec::new()));
    let _file_manager = bus
        .builder()
        .serve_at("/org/freedesktop/FileManager1", FileManager { shown: files.clone() })
        .unwrap()
        .name("org.freedesktop.FileManager1")
        .unwrap()
        .build()
        .await
        .unwrap();
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

    // On the clipboard, and announced; only for the screenshot taken.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let (copied, banners) = {
        let seen = seen.lock().unwrap();
        (seen.copied.clone(), seen.banners.clone())
    };
    assert_eq!(copied, [b"\x89PNG".to_vec()]);
    let (id, summary, buttons) = banners.last().cloned().expect("a notification");
    assert_eq!(summary, "Screenshot captured");
    assert_eq!(buttons, [("show-in-files".to_owned(), "Show in Files".to_owned())]);
    shell
        .ui_bridge()
        .handle(UiRequest::NotificationActivated { id, action: Some("show-in-files".into()) })
        .await
        .unwrap();
    assert_eq!(*files.lock().unwrap(), std::slice::from_ref(&uri));
    std::fs::remove_dir_all(&home).unwrap();
}

fn gio_path(uri: &str) -> std::path::PathBuf {
    let raw = uri.strip_prefix("file://").unwrap();
    std::path::PathBuf::from(raw.replace("%20", " "))
}
