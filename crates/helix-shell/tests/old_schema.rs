//! The shell's settings layer against an older installed copy of Helix's
//! own schemas (helix-desktop-spec `contracts/gsettings/org.helix.shell.md`,
//! "Older installed schemas"): keys the installed schema lacks read as
//! their built-in defaults and are not written, and nothing aborts.

use helix_shell::ext_host::store;
use helix_shell::{notifications, notify_store::IgnoreTimeouts, panel_layout};

/// `xml` without the `<key>` elements named in `drop`.
fn without_keys(xml: &str, drop: &[&str]) -> String {
    let mut out = String::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<key ") {
        let end = rest[start..].find("</key>").map(|e| start + e + "</key>".len()).expect("closed key");
        let element = &rest[start..end];
        out.push_str(&rest[..start]);
        if !drop.iter().any(|k| element.contains(&format!("name=\"{k}\""))) {
            out.push_str(element);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

#[test]
fn older_helix_schemas_do_not_abort() {
    let tmp = std::env::temp_dir().join(format!("helix-old-schema-{}", std::process::id()));
    let schemas = tmp.join("schemas");
    std::fs::create_dir_all(&schemas).unwrap();
    let data = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/");
    let shell = std::fs::read_to_string(format!("{data}org.helix.shell.gschema.xml")).unwrap();
    let exts = std::fs::read_to_string(format!("{data}org.helix.extensions.gschema.xml")).unwrap();
    // As installed by an early package: the panel zones only, and the
    // extension lists without the later keys.
    let old_shell = without_keys(&shell, &["quick-settings", "banner-ignore-timeout", "critical-banner-ignore-timeout", "app-grid-recent"]);
    let old_exts = without_keys(&exts, &["placed", "command-keys", "require-sandbox"]);
    assert!(!old_shell.contains("quick-settings") && old_shell.contains("panel-left"));
    std::fs::write(schemas.join("org.helix.shell.gschema.xml"), old_shell).unwrap();
    std::fs::write(schemas.join("org.helix.extensions.gschema.xml"), old_exts).unwrap();
    let Ok(status) = std::process::Command::new("glib-compile-schemas").arg(&schemas).status() else {
        eprintln!("glib-compile-schemas is not installed; skipping");
        return;
    };
    assert!(status.success());
    // SAFETY: this test binary runs this one test, before any GLib use.
    unsafe {
        std::env::set_var("GSETTINGS_SCHEMA_DIR", &schemas);
        std::env::set_var("GSETTINGS_BACKEND", "memory");
    }
    assert!(helix_settings::has_schema(panel_layout::SCHEMA));
    assert!(helix_settings::key_type(panel_layout::SCHEMA, "quick-settings").is_none());

    // Panel layout: the zones read and write; the quick settings keep
    // their default and are not written.
    let defaults = panel_layout::Keys::default();
    assert_eq!(panel_layout::read(), defaults);
    let mut changed = defaults.clone();
    changed.left = vec!["clock".into()];
    changed.quick = vec!["dark-style".into()];
    panel_layout::write(&changed);
    let read = panel_layout::read();
    assert_eq!(read.left, ["clock"]);
    assert_eq!(read.quick, defaults.quick, "a key the schema lacks keeps its default");

    // Banner ignore periods: the defaults.
    assert_eq!(notifications::read_ignore_timeouts(), IgnoreTimeouts::default());
    // The app grid's "Recent" row: on, as the key's default.
    assert!(helix_settings::key_type(panel_layout::SCHEMA, "app-grid-recent").is_none());
    assert!(helix_shell::app_usage::recent_row_on());

    // Extensions: the old keys round-trip, the newer ones stay default.
    let mut keys = store::Keys::read();
    assert_eq!(keys, store::Keys::default());
    keys.enabled = vec!["org.example.A".into()];
    keys.placed = vec!["org.example.A".into()];
    keys.require_sandbox = true;
    keys.command_keys.insert("org.example.A.cmd".into(), "<Super>j".into());
    keys.write();
    let read = store::Keys::read();
    assert_eq!(read.enabled, ["org.example.A"]);
    assert!(read.placed.is_empty() && read.command_keys.is_empty() && !read.require_sandbox);

    // A typed read of a key that is missing, or asked with another type.
    let s = helix_settings::Schema::open(panel_layout::SCHEMA);
    assert_eq!(s.get::<u32>("banner-ignore-timeout"), None);
    assert_eq!(s.get::<bool>("panel-left"), None, "wrong type reads as missing");
    assert!(!s.set("quick-settings", vec!["x".to_string()]));
    let _ = std::fs::remove_dir_all(&tmp);
}
