//! Launching, session, text size, touchpad, keyboard backlight, battery
//! and custom shortcut keys (helix-desktop-spec
//! `contracts/gsettings/shell-keys.md`).

use gio::prelude::*;
use zbus::Connection;

use crate::Osd;

pub const MEDIA_KEYS: &str = "org.gnome.settings-daemon.plugins.media-keys";
pub const CUSTOM: &str = "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchKey {
    Www,
    Email,
    Calculator,
    Settings,
    Home,
    Media,
    Help,
    Search,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKey {
    Lock,
    Logout,
    Shutdown,
    Reboot,
    Suspend,
    Hibernate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKey {
    Bigger,
    Smaller,
    Contrast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    On,
    Off,
    Flip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KbdKey {
    Up,
    Down,
    Toggle,
}

fn settings_with(schema: &str, key: &str) -> Option<gio::Settings> {
    let found = gio::SettingsSchemaSource::default()?.lookup(schema, true)?;
    found.has_key(key).then(|| gio::Settings::new(schema))
}

// ----- Launching --------------------------------------------------------

fn launch_app(app: &gio::AppInfo, uri: Option<&str>) {
    let uris: Vec<&str> = uri.into_iter().collect();
    if let Err(err) = app.launch_uris(&uris, None::<&gio::AppLaunchContext>) {
        tracing::warn!(%err, app = %app.name(), "cannot launch");
    }
}

fn default_for(mime: &str) -> Option<gio::AppInfo> {
    gio::AppInfo::default_for_type(mime, false)
}

fn desktop_app(id: &str) -> Option<gio::AppInfo> {
    let file = format!("{id}.desktop");
    gio::AppInfo::all().into_iter().find(|a| a.id().is_some_and(|i| i == file.as_str()))
}

/// The calculator: `org.gnome.Calculator`, else an app in the
/// `Calculator` category.
fn calculator() -> Option<gio::AppInfo> {
    desktop_app("org.gnome.Calculator").or_else(|| {
        gio::AppInfo::all()
            .into_iter()
            .find(|a| a.id().is_some_and(|id| crate::favorites::categories(id.as_str()).iter().any(|c| c == "Calculator")))
    })
}

/// Open what a launch key names; `Search` is the UI's (false returned).
pub fn launch(key: LaunchKey) -> bool {
    let (app, uri) = match key {
        LaunchKey::Www => (default_for("x-scheme-handler/https"), None),
        LaunchKey::Email => (default_for("x-scheme-handler/mailto"), None),
        LaunchKey::Calculator => (calculator(), None),
        LaunchKey::Settings => (desktop_app("org.gnome.Settings"), None),
        LaunchKey::Media => (default_for("audio/x-vorbis+ogg"), None),
        LaunchKey::Home => {
            let uri = gio::File::for_path(gio::glib::home_dir()).uri().to_string();
            (default_for("inode/directory"), Some(uri))
        }
        LaunchKey::Help => {
            if let Err(err) = gio::AppInfo::launch_default_for_uri("help:gnome-help", None::<&gio::AppLaunchContext>) {
                tracing::info!(%err, "no help viewer");
            }
            return true;
        }
        LaunchKey::Search => return false,
    };
    match app {
        Some(app) => launch_app(&app, uri.as_deref()),
        None => tracing::info!(?key, "no app for this key"),
    }
    true
}

// ----- Session ----------------------------------------------------------

/// Session keys other than `Lock` (the caller locks).
pub async fn session(conn: &Connection, key: SessionKey) {
    let sm = |member: &'static str, mode: Option<u32>| async move {
        let (name, path) = ("org.gnome.SessionManager", "/org/gnome/SessionManager");
        match mode {
            Some(mode) => conn.call_method(Some(name), path, Some(name), member, &(mode,)).await,
            None => conn.call_method(Some(name), path, Some(name), member, &()).await,
        }
        .map(drop)
    };
    let result = match key {
        SessionKey::Lock => Ok(()),
        SessionKey::Logout => sm("Logout", Some(0)).await,
        SessionKey::Shutdown => sm("Shutdown", None).await,
        SessionKey::Reboot => sm("Reboot", None).await,
        SessionKey::Suspend => sm("Suspend", None).await,
        SessionKey::Hibernate => {
            crate::power::hibernate(conn).await;
            Ok(())
        }
    };
    if let Err(err) = result {
        tracing::warn!(%err, ?key, "session key failed");
    }
}

// ----- Text size, contrast, touchpad -----------------------------------

/// The next `text-scaling-factor`.
pub fn text_step(factor: f64, bigger: bool) -> f64 {
    let next = if bigger { factor + 0.25 } else { factor - 0.25 };
    (next * 4.0).round() / 4.0
}

pub fn text(key: TextKey) {
    match key {
        TextKey::Bigger | TextKey::Smaller => {
            if let Some(s) = settings_with("org.gnome.desktop.interface", "text-scaling-factor") {
                let next = text_step(s.double("text-scaling-factor"), key == TextKey::Bigger).clamp(0.5, 3.0);
                let _ = s.set_double("text-scaling-factor", next);
            }
        }
        TextKey::Contrast => {
            if let Some(s) = settings_with("org.gnome.desktop.a11y.interface", "high-contrast") {
                let _ = s.set_boolean("high-contrast", !s.boolean("high-contrast"));
            }
        }
    }
    gio::Settings::sync();
}

/// Turn the touchpad on or off; the OSD to show.
pub fn touchpad(key: Toggle) -> Option<Osd> {
    let s = settings_with("org.gnome.desktop.peripherals.touchpad", "send-events")?;
    let on = match key {
        Toggle::On => true,
        Toggle::Off => false,
        Toggle::Flip => s.string("send-events") != "enabled",
    };
    let _ = s.set_string("send-events", if on { "enabled" } else { "disabled" });
    gio::Settings::sync();
    Some(Osd {
        icon: Some(if on { "input-touchpad-symbolic" } else { "touchpad-disabled-symbolic" }.into()),
        label: Some(if on { "Touchpad On" } else { "Touchpad Off" }.into()),
        level: None,
        max_level: 1.0,
        connector: None,
    })
}

/// `org.gnome.settings-daemon.peripherals.touchscreen`.
pub const TOUCHSCREEN: &str = "org.gnome.settings-daemon.peripherals.touchscreen";

/// Whether automatic rotation is locked.
pub fn orientation_locked() -> bool {
    settings_with(TOUCHSCREEN, "orientation-lock").is_some_and(|s| s.boolean("orientation-lock"))
}

/// Flip `orientation-lock`.
pub fn toggle_orientation_lock() {
    if let Some(s) = settings_with(TOUCHSCREEN, "orientation-lock") {
        let _ = s.set_boolean("orientation-lock", !s.boolean("orientation-lock"));
        gio::Settings::sync();
    }
}

// ----- Keyboard backlight, battery -------------------------------------

const UPOWER: &str = "org.freedesktop.UPower";
const KBD_PATH: &str = "/org/freedesktop/UPower/KbdBacklight";
const KBD: &str = "org.freedesktop.UPower.KbdBacklight";

/// The next keyboard backlight value; `last` is the last non-zero one.
pub fn kbd_step(value: i32, max: i32, last: i32, key: KbdKey) -> i32 {
    let step = (max / 10).max(1);
    match key {
        KbdKey::Up => (value + step).min(max),
        KbdKey::Down => (value - step).max(0),
        KbdKey::Toggle => {
            if value > 0 {
                0
            } else if last > 0 {
                last.min(max)
            } else {
                max
            }
        }
    }
}

/// The last non-zero keyboard backlight value (for the toggle).
static KBD_LAST: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Change the keyboard backlight on `system`; the OSD to show.
pub async fn keyboard_backlight(system: &Connection, key: KbdKey) -> Option<Osd> {
    let call = |member: &'static str| async move {
        system.call_method(Some(UPOWER), KBD_PATH, Some(KBD), member, &()).await.ok()?.body().deserialize::<i32>().ok()
    };
    let max = call("GetMaxBrightness").await.filter(|m| *m > 0)?;
    let value = call("GetBrightness").await?;
    use std::sync::atomic::Ordering;
    if value > 0 {
        KBD_LAST.store(value, Ordering::Relaxed);
    }
    let next = kbd_step(value, max, KBD_LAST.load(Ordering::Relaxed), key);
    if let Err(err) = system.call_method(Some(UPOWER), KBD_PATH, Some(KBD), "SetBrightness", &(next,)).await {
        tracing::warn!(%err, "cannot set the keyboard backlight");
        return None;
    }
    Some(Osd {
        icon: Some("keyboard-brightness-symbolic".into()),
        label: None,
        level: Some(f64::from(next) / f64::from(max)),
        max_level: 1.0,
        connector: None,
    })
}

/// The battery's icon and level, if there is one.
pub async fn battery(system: &Connection) -> Option<Osd> {
    let proxy = zbus::Proxy::new(system, UPOWER, "/org/freedesktop/UPower/devices/DisplayDevice", "org.freedesktop.UPower.Device").await.ok()?;
    if !proxy.get_property::<bool>("IsPresent").await.ok()? {
        return None;
    }
    let percent: f64 = proxy.get_property("Percentage").await.ok()?;
    let icon: String = proxy.get_property("IconName").await.unwrap_or_default();
    Some(Osd {
        icon: Some(if icon.is_empty() { "battery-symbolic".into() } else { icon }),
        label: None,
        level: Some(percent / 100.0),
        max_level: 1.0,
        connector: None,
    })
}

// ----- Custom shortcuts -------------------------------------------------

/// A custom shortcut: (binding, command).
pub type Custom = (String, String);

/// The custom shortcuts, in `custom-keybindings` order.
pub fn customs() -> Vec<Custom> {
    let Some(list) = settings_with(MEDIA_KEYS, "custom-keybindings") else { return Vec::new() };
    let Some(schema) = gio::SettingsSchemaSource::default().and_then(|s| s.lookup(CUSTOM, true)) else { return Vec::new() };
    list.strv("custom-keybindings")
        .iter()
        .filter(|p| p.starts_with('/') && p.ends_with('/'))
        .map(|path| {
            let s = gio::Settings::new_full(&schema, None::<&gio::SettingsBackend>, Some(path.as_str()));
            (s.string("binding").to_string(), s.string("command").to_string())
        })
        .collect()
}

/// Run custom shortcut `index` (of [`customs`]).
pub fn run_custom(index: usize) {
    let Some((_, command)) = customs().into_iter().nth(index) else { return };
    let argv = match gio::glib::shell_parse_argv(&command) {
        Ok(argv) if !argv.is_empty() => argv,
        _ => {
            tracing::warn!(command, "cannot parse the custom shortcut's command");
            return;
        }
    };
    let argv: Vec<String> = argv.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    match std::process::Command::new(&argv[0]).args(&argv[1..]).spawn() {
        Ok(child) => tracing::info!(command, pid = child.id(), "custom shortcut"),
        Err(err) => tracing::warn!(%err, command, "cannot run the custom shortcut"),
    }
}

/// Follow key settings (and custom shortcut paths); `changed` runs on
/// every change. Runs a GLib loop on its own thread until the process ends.
pub fn follow(schemas: &'static [&'static str], changed: impl Fn() + Send + 'static) {
    let spawned = std::thread::Builder::new().name("helix-key-settings".into()).spawn(move || {
        let context = gio::glib::MainContext::new();
        let main_loop = gio::glib::MainLoop::new(Some(&context), false);
        let _ = context.with_thread_default(|| {
            let changed = std::rc::Rc::new(changed);
            let mut keep: Vec<gio::Settings> = Vec::new();
            let watch = |s: gio::Settings, keep: &mut Vec<gio::Settings>| {
                let notify = changed.clone();
                s.connect_changed(None, move |_, _| notify());
                for key in s.settings_schema().map(|x| x.list_keys()).unwrap_or_default() {
                    let _ = s.value(&key);
                }
                keep.push(s);
            };
            let source = gio::SettingsSchemaSource::default();
            for id in schemas {
                if source.as_ref().and_then(|s| s.lookup(id, true)).is_some() {
                    watch(gio::Settings::new(id), &mut keep);
                }
            }
            // Custom shortcuts: their paths, re-read when the list changes
            // (watching paths that went away is harmless).
            let custom_schema = source.as_ref().and_then(|s| s.lookup(CUSTOM, true));
            let watched = std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashSet::<String>::new()));
            let keep = std::rc::Rc::new(std::cell::RefCell::new(keep));
            let add_paths = {
                let (keep, watched, changed) = (keep.clone(), watched.clone(), changed.clone());
                move || {
                    let (Some(schema), Some(list)) = (custom_schema.as_ref(), settings_with(MEDIA_KEYS, "custom-keybindings")) else { return };
                    for path in list.strv("custom-keybindings").iter().filter(|p| p.starts_with('/') && p.ends_with('/')) {
                        if watched.borrow_mut().insert(path.to_string()) {
                            let s = gio::Settings::new_full(schema, None::<&gio::SettingsBackend>, Some(path.as_str()));
                            let notify = changed.clone();
                            s.connect_changed(None, move |_, _| notify());
                            for key in ["name", "command", "binding"] {
                                let _ = s.value(key);
                            }
                            keep.borrow_mut().push(s);
                        }
                    }
                }
            };
            add_paths();
            if let Some(list) = settings_with(MEDIA_KEYS, "custom-keybindings") {
                let add = add_paths.clone();
                list.connect_changed(Some("custom-keybindings"), move |_, _| add());
                let _ = list.value("custom-keybindings");
                keep.borrow_mut().push(list);
            }
            main_loop.run();
        });
    });
    if let Err(err) = spawned {
        tracing::warn!(%err, "cannot follow key settings");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps() {
        assert_eq!(text_step(1.0, true), 1.25);
        assert_eq!(text_step(1.1, false), 0.75, "rounded to quarters");
        assert_eq!(kbd_step(1, 3, 0, KbdKey::Up), 2);
        assert_eq!(kbd_step(3, 3, 0, KbdKey::Up), 3);
        assert_eq!(kbd_step(0, 3, 0, KbdKey::Down), 0);
        assert_eq!(kbd_step(50, 100, 0, KbdKey::Down), 40);
        assert_eq!(kbd_step(2, 3, 2, KbdKey::Toggle), 0);
        assert_eq!(kbd_step(0, 3, 2, KbdKey::Toggle), 2, "back to the last value");
        assert_eq!(kbd_step(0, 3, 0, KbdKey::Toggle), 3);
    }
}
