//! `org.freedesktop.impl.portal.Lockdown` (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.impl.portal.md`, "Lockdown"): the
//! administrator's restrictions, from GSettings.

use std::sync::Mutex;

use gio::prelude::*;
use zbus::{fdo, interface};

/// Each property: its name, schema, key, and whether the key means the
/// opposite (`enabled` against `disable-location`).
pub const KEYS: [(&str, &str, &str, bool); 7] = [
    ("disable-printing", "org.gnome.desktop.lockdown", "disable-printing", false),
    ("disable-save-to-disk", "org.gnome.desktop.lockdown", "disable-save-to-disk", false),
    ("disable-application-handlers", "org.gnome.desktop.lockdown", "disable-application-handlers", false),
    ("disable-location", "org.gnome.system.location", "enabled", true),
    ("disable-camera", "org.gnome.desktop.privacy", "disable-camera", false),
    ("disable-microphone", "org.gnome.desktop.privacy", "disable-microphone", false),
    ("disable-sound-output", "org.gnome.desktop.privacy", "disable-sound-output", false),
];

/// The schemas to watch.
pub const SCHEMAS: &[&str] = &["org.gnome.desktop.lockdown", "org.gnome.system.location", "org.gnome.desktop.privacy"];

fn entry(property: &str) -> &'static (&'static str, &'static str, &'static str, bool) {
    KEYS.iter().find(|k| k.0 == property).expect("known property")
}

/// The property's value now (false when the schema is missing).
pub fn read(property: &str) -> bool {
    let (_, schema, key, invert) = *entry(property);
    helix_settings::has_schema(schema) && gio::Settings::new(schema).boolean(key) != invert
}

fn write(property: &str, value: bool) -> fdo::Result<()> {
    let (_, schema, key, invert) = *entry(property);
    if !helix_settings::has_schema(schema) {
        return Err(fdo::Error::NotSupported(format!("{schema} is not installed")));
    }
    let settings = gio::Settings::new(schema);
    settings.set_boolean(key, value != invert).map_err(|e| fdo::Error::AccessDenied(e.to_string()))?;
    gio::Settings::sync();
    Ok(())
}

#[derive(Default)]
pub struct LockdownBackend {
    /// The values last announced.
    last: Mutex<Option<[bool; 7]>>,
}

macro_rules! lockdown_properties {
    ($(($get:ident, $set:ident, $name:literal)),* $(,)?) => {
        #[interface(name = "org.freedesktop.impl.portal.Lockdown")]
        impl LockdownBackend {
            $(
                #[zbus(property, name = $name)]
                fn $get(&self) -> bool {
                    read($name)
                }

                #[zbus(property, name = $name)]
                fn $set(&self, value: bool) -> fdo::Result<()> {
                    write($name, value)?;
                    // The setter announces it; the settings watcher need not.
                    if let Some(last) = self.last.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                        last[KEYS.iter().position(|k| k.0 == $name).expect("known property")] = value;
                    }
                    Ok(())
                }
            )*
        }
    };
}

lockdown_properties!(
    (disable_printing, set_disable_printing, "disable-printing"),
    (disable_save_to_disk, set_disable_save_to_disk, "disable-save-to-disk"),
    (disable_application_handlers, set_disable_application_handlers, "disable-application-handlers"),
    (disable_location, set_disable_location, "disable-location"),
    (disable_camera, set_disable_camera, "disable-camera"),
    (disable_microphone, set_disable_microphone, "disable-microphone"),
    (disable_sound_output, set_disable_sound_output, "disable-sound-output"),
);

/// Which properties differ between `before` and `now` (all, the first time).
pub fn changed(before: Option<[bool; 7]>, now: [bool; 7]) -> Vec<&'static str> {
    KEYS.iter().enumerate().filter(|(i, _)| before.is_none_or(|b| b[*i] != now[*i])).map(|(_, k)| k.0).collect()
}

/// Announce the properties that changed since last time.
pub async fn refresh(conn: &zbus::Connection, path: &str) -> zbus::Result<()> {
    let iface = conn.object_server().interface::<_, LockdownBackend>(path).await?;
    let now = KEYS.map(|k| read(k.0));
    let names = {
        let backend = iface.get().await;
        let mut last = backend.last.lock().unwrap_or_else(|e| e.into_inner());
        let first = last.is_none();
        let names = changed(*last, now);
        *last = Some(now);
        if first {
            // Nobody heard the old values: nothing to announce.
            return Ok(());
        }
        names
    };
    let backend = iface.get().await;
    let emitter = iface.signal_emitter();
    for name in names {
        match name {
            "disable-printing" => backend.disable_printing_changed(emitter).await?,
            "disable-save-to-disk" => backend.disable_save_to_disk_changed(emitter).await?,
            "disable-application-handlers" => backend.disable_application_handlers_changed(emitter).await?,
            "disable-location" => backend.disable_location_changed(emitter).await?,
            "disable-camera" => backend.disable_camera_changed(emitter).await?,
            "disable-microphone" => backend.disable_microphone_changed(emitter).await?,
            _ => backend.disable_sound_output_changed(emitter).await?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes() {
        let a = [false; 7];
        let mut b = a;
        b[3] = true;
        assert_eq!(changed(Some(a), b), ["disable-location"]);
        assert_eq!(changed(None, a).len(), 7);
        assert!(changed(Some(a), a).is_empty());
    }
}
