//! The app grid's "Recent" row and its settings (helix-desktop-spec
//! `contracts/gsettings/org.helix.shell.md`, `app-grid-recent`, and
//! `contracts/gsettings/org.gnome.desktop.privacy.md`, "App usage"):
//! launches are kept as recent apps only while `remember-app-usage` is
//! on, the row shows only while both keys are on, and the shell is told
//! at once when either changes. Runs in its own process, with Helix's
//! schema compiled into a scratch directory, the memory settings backend
//! and a scratch data directory.

use std::time::Duration;

use helix_shell::app_usage;
use helix_shell::UiEvent;

const PRIVACY: &str = "org.gnome.desktop.privacy";

/// The next event the shell is sent (none within 5 seconds: `None`).
async fn next(rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiEvent>) -> Option<UiEvent> {
    tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.ok().flatten()
}

#[test]
fn recent_row_follows_its_settings() {
    let tmp = std::env::temp_dir().join(format!("helix-recent-row-{}", std::process::id()));
    let schemas = tmp.join("schemas");
    std::fs::create_dir_all(&schemas).unwrap();
    let data = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/org.helix.shell.gschema.xml");
    std::fs::copy(data, schemas.join("org.helix.shell.gschema.xml")).unwrap();
    let Ok(status) = std::process::Command::new("glib-compile-schemas").arg(&schemas).status() else {
        eprintln!("glib-compile-schemas is not installed; skipping");
        return;
    };
    assert!(status.success());
    // SAFETY: this test binary runs this one test, before any GLib use.
    unsafe {
        std::env::set_var("GSETTINGS_SCHEMA_DIR", &schemas);
        std::env::set_var("GSETTINGS_BACKEND", "memory");
        std::env::set_var("XDG_DATA_HOME", tmp.join("data"));
    }
    helix_settings::prepare_gio();
    let has_privacy = helix_settings::has_schema(PRIVACY);
    let shell = helix_settings::Schema::open("org.helix.shell");
    assert_eq!(shell.get::<bool>("app-grid-recent"), Some(true), "on by default");
    assert!(app_usage::recent_row_on());

    // Launches are kept, the latest first, each once.
    for id in ["a.desktop", "b.desktop", "a.desktop", "c.desktop"] {
        app_usage::record(id);
    }
    assert_eq!(app_usage::recent(), ["c.desktop", "a.desktop", "b.desktop"]);
    let file = tmp.join("data/helix/recent-apps");
    assert!(file.exists());

    // Followed live: the shell hears of each change.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    rt.block_on(async {
        tokio::spawn(app_usage::follow_recent_row(tx));
        assert_eq!(next(&mut rx).await, Some(UiEvent::RecentApps(true)), "told at start");
        // Turned off in Helix's schema.
        assert!(shell.set("app-grid-recent", false));
        assert!(!app_usage::recent_row_on());
        assert_eq!(next(&mut rx).await, Some(UiEvent::RecentApps(false)), "told when turned off");
        assert_eq!(app_usage::recent(), ["c.desktop", "a.desktop", "b.desktop"], "still kept: only not shown");
        assert!(shell.set("app-grid-recent", true));
        assert_eq!(next(&mut rx).await, Some(UiEvent::RecentApps(true)));
        if has_privacy {
            // Usage not remembered: no row, nothing kept, the record gone.
            let privacy = helix_settings::Schema::open(PRIVACY);
            assert!(privacy.set("remember-app-usage", false));
            assert!(!app_usage::recent_row_on());
            assert_eq!(next(&mut rx).await, Some(UiEvent::RecentApps(false)), "told when usage is not remembered");
            app_usage::record("d.desktop");
            assert!(app_usage::recent().is_empty());
            assert!(!file.exists(), "the record is deleted");
            assert!(privacy.set("remember-app-usage", true));
            assert_eq!(next(&mut rx).await, Some(UiEvent::RecentApps(true)));
            assert!(app_usage::recent().is_empty(), "nothing was kept meanwhile");
        } else {
            eprintln!("{PRIVACY} is not installed; remember-app-usage not checked");
        }
    });

    // Cleared: gone, and kept again from the next launch.
    app_usage::record("e.desktop");
    app_usage::record("f.desktop");
    app_usage::clear_recent();
    assert!(app_usage::recent().is_empty());
    app_usage::record("g.desktop");
    assert_eq!(app_usage::recent(), ["g.desktop"]);
    let _ = std::fs::remove_dir_all(&tmp);
}
