//! Autostart follows `AutostartCondition=GSettings …` (helix-desktop-spec
//! `org.gnome.SessionManager.md`, "Autostart").

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use gio::prelude::*;

const SCHEMA: &str = "org.gnome.desktop.a11y.applications";
const KEY: &str = "screen-reader-enabled";

async fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..100 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

fn alive(pid_file: &Path) -> bool {
    let Ok(pid) = std::fs::read_to_string(pid_file) else { return false };
    Path::new(&format!("/proc/{}", pid.trim())).exists()
}

#[tokio::test]
async fn follows_gsettings_conditions() {
    let has_key = gio::SettingsSchemaSource::default().and_then(|s| s.lookup(SCHEMA, true)).is_some_and(|s| s.has_key(KEY));
    if !has_key {
        return;
    }
    let base = std::env::temp_dir().join(format!("helix-autostart-it-{}", std::process::id()));
    let autostart = base.join("config/autostart");
    std::fs::create_dir_all(&autostart).unwrap();
    let pid_file = base.join("reader.pid");
    std::fs::write(
        autostart.join("reader.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName=Reader\nExec=sh -c \"echo $$ > {} && exec sleep 60\"\nAutostartCondition=GSettings {SCHEMA} {KEY}\n",
            pid_file.display()
        ),
    )
    .unwrap();
    // SAFETY: this test binary runs this one test.
    unsafe {
        std::env::set_var("GSETTINGS_BACKEND", "memory");
        std::env::set_var("XDG_CONFIG_HOME", base.join("config"));
        std::env::set_var("XDG_CONFIG_DIRS", base.join("none"));
    }
    let settings = gio::Settings::new(SCHEMA);
    settings.set_boolean(KEY, false).unwrap();

    let apps = helix_session::autostart::Autostart::new(BTreeMap::new());
    assert_eq!(apps.schemas(), [SCHEMA]);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(apps.run(async {
        let _ = stop_rx.await;
    }));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!pid_file.exists(), "not started while the key is false");

    settings.set_boolean(KEY, true).unwrap();
    wait_for("the reader to start", || alive(&pid_file)).await;
    settings.set_boolean(KEY, false).unwrap();
    wait_for("the reader to stop", || !alive(&pid_file)).await;

    let _ = stop_tx.send(());
    task.await.unwrap();
    std::fs::remove_dir_all(&base).unwrap();
}
