//! The shell's readers of schemas Helix does not own (GNOME's, Ubuntu's)
//! against installed copies that lack the keys it reads: every schema
//! here is installed with one unrelated key only, as an older GNOME
//! release (or a partial upgrade) could leave them. GLib aborts the
//! process when asked for a key its schema lacks; the shell's reads go
//! through `helix_settings::Settings`, so they get empty values instead
//! and this test binary survives them all.

use helix_shell::*;

/// Schemas the shell reads, each installed without the keys it reads.
const SCHEMAS: &[&str] = &[
    "org.gnome.desktop.interface",
    "org.gnome.desktop.screensaver",
    "org.gnome.desktop.session",
    "org.gnome.desktop.privacy",
    "org.gnome.desktop.notifications",
    "org.gnome.desktop.lockdown",
    "org.gnome.desktop.media-handling",
    "org.gnome.desktop.input-sources",
    "org.gnome.desktop.peripherals.mouse",
    "org.gnome.desktop.wm.keybindings",
    "org.gnome.desktop.wm.preferences",
    "org.gnome.desktop.background",
    "org.gnome.shell",
    "org.gnome.shell.keybindings",
    "org.gnome.shell.app-switcher",
    "org.gnome.shell.window-switcher",
    "org.gnome.shell.world-clocks",
    "org.gnome.shell.weather",
    "org.gnome.shell.extensions.dash-to-dock",
    "org.gnome.mutter",
    "org.gnome.mutter.keybindings",
    "org.gnome.settings-daemon.plugins.color",
    "org.gnome.settings-daemon.plugins.power",
    "org.gnome.settings-daemon.plugins.media-keys",
    "org.gnome.settings-daemon.plugins.housekeeping",
    "org.gnome.system.location",
    "org.gnome.GWeather4",
];

#[test]
fn older_gnome_schemas_do_not_abort() {
    let tmp = std::env::temp_dir().join(format!("helix-old-gnome-schema-{}", std::process::id()));
    let schemas = tmp.join("schemas");
    std::fs::create_dir_all(&schemas).unwrap();
    for id in SCHEMAS {
        let path = format!("/{}/", id.replace('.', "/"));
        let xml = format!(
            "<schemalist><schema id=\"{id}\" path=\"{path}\"><key name=\"helix-test-only\" type=\"b\"><default>false</default></key></schema></schemalist>\n"
        );
        std::fs::write(schemas.join(format!("{id}.gschema.xml")), xml).unwrap();
    }
    let Ok(status) = std::process::Command::new("glib-compile-schemas").arg(&schemas).status() else {
        eprintln!("glib-compile-schemas is not installed; skipping");
        return;
    };
    assert!(status.success());
    // SAFETY: this test binary runs this one test, before any GLib use.
    unsafe {
        std::env::set_var("GSETTINGS_SCHEMA_DIR", &schemas);
        std::env::set_var("GSETTINGS_BACKEND", "memory");
        // Only the stubs: not the host's schemas.
        std::env::set_var("XDG_DATA_DIRS", tmp.join("no-data"));
        std::env::set_var("XDG_DATA_HOME", tmp.join("no-data-home"));
    }
    assert!(helix_settings::has_schema("org.gnome.desktop.interface"));
    assert!(helix_settings::key_type("org.gnome.desktop.interface", "clock-format").is_none());

    // Each of these reads keys the stubs lack.
    let _ = clock::read();
    let _ = dock::read();
    let _ = idle::lock_on_suspend();
    let _ = item_feedback::animations_enabled();
    let _ = item_feedback::motion();
    let _ = app_notify::policy("org.example.App");
    let _ = calendar::world_clocks();
    let _ = geoclue_agent::max_level();
    let _ = input_sources::per_window();
    let _ = drag_threshold();
    let _ = screensaver_flag("lock-enabled", true);
    let _ = locked_down("disable-lock-screen");
    let _ = mount_handler::remember_default();
    let _ = night_light::read();
    let _ = usb_protection::read();
    let _ = user_switch::on_lock_screen();
    let _ = weather::unit();
    let _ = windows::wm_key_accelerators();
    let _ = windows::accelerators();
    let _ = windows::workspace_accelerators();
    let _ = windows::current_workspace_only();
    let _ = windows::icon_mode();
    let _ = housekeeping::read();
    let _ = app_usage::enabled();
    let _ = app_grid::read();
    let _ = favorites::favorites();
    let _ = wallpaper::Background::default();
    // Writes to keys the schema lacks are skipped, not fatal.
    mount_handler::remember_chosen(true);
    status::remember_profile("balanced");
    let s = helix_settings::Settings::new("org.gnome.desktop.interface");
    assert!(!s.boolean("clock-show-date") && s.string("clock-format").is_empty() && s.strv("x").is_empty());
    assert!(s.set_boolean("clock-show-date", true).is_err());
    let missing = helix_settings::Settings::new("org.example.NotInstalled");
    assert_eq!((missing.int("a"), missing.double("b")), (0, 0.0));
    let _ = std::fs::remove_dir_all(&tmp);
}
