//! Reads and watches the real installed schemas, using GSettings' in-memory
//! backend so the user's dconf database is never touched.

use std::time::Duration;

use gio::prelude::*;
use helix_settings::{key_type, GSettings, PORTAL_SCHEMAS};

const INTERFACE: &str = "org.gnome.desktop.interface";

fn use_memory_backend() -> bool {
    // Must happen before GIO picks a backend; this test binary does nothing
    // else with GSettings first.
    std::env::set_var("GSETTINGS_BACKEND", "memory");
    if helix_settings::has_schema(INTERFACE) {
        true
    } else {
        eprintln!("{INTERFACE} schema not installed; skipping");
        false
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_and_watch() {
    if !use_memory_backend() {
        return;
    }
    assert_eq!(key_type(INTERFACE, "color-scheme").as_deref(), Some("s"));
    assert_eq!(key_type(INTERFACE, "no-such-key"), None);
    assert_eq!(key_type("org.example.NoSuchSchema", "x"), None);

    let settings = GSettings::new(&[INTERFACE, "org.example.NoSuchSchema"]);
    assert_eq!(settings.schemas(), [INTERFACE.to_owned()]);

    let snapshot = settings.snapshot();
    let scheme = &snapshot[INTERFACE]["color-scheme"];
    assert_eq!(<&str>::try_from(&**scheme).unwrap(), "default");

    let (mut changes, _watch) = settings.watch();
    gio::Settings::new(INTERFACE)
        .set_string("color-scheme", "prefer-dark")
        .unwrap();
    let changed = tokio::time::timeout(Duration::from_secs(5), changes.recv())
        .await
        .expect("change notification")
        .unwrap();
    assert_eq!(changed, INTERFACE);

    let snapshot = settings.snapshot();
    let scheme = &snapshot[INTERFACE]["color-scheme"];
    assert_eq!(<&str>::try_from(&**scheme).unwrap(), "prefer-dark");
}

#[test]
fn portal_schemas_are_known() {
    if !use_memory_backend() {
        return;
    }
    let settings = GSettings::new(PORTAL_SCHEMAS);
    let snapshot = settings.snapshot();
    for id in settings.schemas() {
        assert!(!snapshot[id].is_empty(), "{id} has no readable keys");
    }
}
