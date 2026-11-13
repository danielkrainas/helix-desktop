//! Per-application notification settings (helix-desktop-spec
//! `org.freedesktop.Notifications.md`, "Per-application settings").

use gio::prelude::*;

const NOTIFICATIONS: &str = "org.gnome.desktop.notifications";
const APPLICATION: &str = "org.gnome.desktop.notifications.application";
const BASE: &str = "/org/gnome/desktop/notifications/application/";

/// What an app's settings allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppPolicy {
    pub enable: bool,
    pub banners: bool,
    pub sounds: bool,
}

impl Default for AppPolicy {
    fn default() -> Self {
        Self { enable: true, banners: true, sounds: true }
    }
}

/// The child name of `app_id`'s settings.
pub fn child(app_id: &str) -> String {
    app_id.to_lowercase().chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' { c } else { '-' }).collect()
}

/// The app id of a `desktop-entry` hint.
pub fn app_id_of_entry(entry: &str) -> &str {
    entry.strip_suffix(".desktop").unwrap_or(entry)
}

/// `app_id`'s settings; the first time, the app is added to
/// `application-children` (so GNOME Settings lists it).
pub fn policy(app_id: &str) -> AppPolicy {
    if app_id.is_empty() {
        return AppPolicy::default();
    }
    let source = gio::SettingsSchemaSource::default();
    let (Some(app_schema), Some(_)) = (
        source.as_ref().and_then(|s| s.lookup(APPLICATION, true)),
        source.as_ref().and_then(|s| s.lookup(NOTIFICATIONS, true)),
    ) else {
        return AppPolicy::default();
    };
    let name = child(app_id);
    let settings = gio::Settings::new_full(&app_schema, None::<&gio::SettingsBackend>, Some(format!("{BASE}{name}/").as_str()));
    let parent = gio::Settings::new(NOTIFICATIONS);
    let children: Vec<String> = parent.strv("application-children").iter().map(|s| s.to_string()).collect();
    if !children.contains(&name) {
        let mut next: Vec<&str> = children.iter().map(String::as_str).collect();
        next.push(&name);
        let _ = parent.set_strv("application-children", next.as_slice());
        let _ = settings.set_string("application-id", &format!("{app_id}.desktop"));
        gio::Settings::sync();
        tracing::info!(app_id, child = name, "new notifying app");
    }
    AppPolicy {
        enable: settings.boolean("enable"),
        banners: settings.boolean("show-banners"),
        sounds: settings.boolean("enable-sound-alerts"),
    }
}

/// `force-expanded`: `app_id`'s banners show their whole text (read only;
/// false without settings).
pub fn force_expanded(app_id: &str) -> bool {
    let source = gio::SettingsSchemaSource::default();
    let Some(app_schema) = source.as_ref().and_then(|s| s.lookup(APPLICATION, true)).filter(|_| !app_id.is_empty()) else {
        return false;
    };
    if !app_schema.has_key("force-expanded") {
        return false;
    }
    let settings = gio::Settings::new_full(&app_schema, None::<&gio::SettingsBackend>, Some(format!("{BASE}{}/", child(app_id)).as_str()));
    settings.boolean("force-expanded")
}

/// Whether `app_id`'s notifications show on the lock screen, and with
/// their text: (`show-in-lock-screen`, `details-in-lock-screen`). Read
/// only; apps without settings get (true, false).
pub fn lock_policy(app_id: &str) -> (bool, bool) {
    let source = gio::SettingsSchemaSource::default();
    let Some(app_schema) = source.as_ref().and_then(|s| s.lookup(APPLICATION, true)).filter(|_| !app_id.is_empty()) else {
        return (true, false);
    };
    let settings = gio::Settings::new_full(&app_schema, None::<&gio::SettingsBackend>, Some(format!("{BASE}{}/", child(app_id)).as_str()));
    (settings.boolean("show-in-lock-screen"), settings.boolean("details-in-lock-screen"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(child("org.gnome.Calendar"), "org-gnome-calendar");
        assert_eq!(child("firefox_firefox"), "firefox-firefox");
        assert_eq!(child("My App 2"), "my-app-2");
        assert_eq!(app_id_of_entry("org.gnome.Calendar.desktop"), "org.gnome.Calendar");
        assert_eq!(app_id_of_entry("firefox"), "firefox");
    }
}
