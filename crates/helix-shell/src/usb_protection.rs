//! USB protection (helix-desktop-spec `contracts/gsettings/org.gnome.desktop.privacy.md`,
//! "USB protection"): with `usb-protection`, USB devices plugged in while
//! the screen is locked (`usb-protection-level` `lockscreen`) or at any
//! time (`always`) are rejected through USBGuard; keyboards are left
//! alone.
//!
//! Only on bare hardware (`HELIX_BACKEND=drm`): USBGuard acts on this
//! machine's devices. `HELIX_TEST_USBGUARD=1` looks for it on the session
//! bus instead (tests with a fake).

use futures_util::StreamExt;
use zbus::Connection;

const SCHEMA: &str = "org.gnome.desktop.privacy";
const USBGUARD: &str = "org.usbguard1";
const DEVICES_PATH: &str = "/org/usbguard1/Devices";
const DEVICES: &str = "org.usbguard.Devices1";
/// `DevicePresenceChanged` event: inserted.
const INSERT: u32 = 1;
/// `applyDevicePolicy` targets.
pub const ALLOW: u32 = 0;
pub const REJECT: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    LockScreen,
    Always,
}

/// The settings: `None` while protection is off.
pub fn read() -> Option<Level> {
    use gio::prelude::*;
    if !helix_settings::has_schema(SCHEMA) {
        return None;
    }
    let s = gio::Settings::new(SCHEMA);
    let has = |k: &str| s.settings_schema().is_some_and(|x| x.has_key(k));
    if !has("usb-protection") || !s.boolean("usb-protection") {
        return None;
    }
    Some(if has("usb-protection-level") && s.string("usb-protection-level") == "always" { Level::Always } else { Level::LockScreen })
}

/// Whether a device (its USBGuard rule) is a keyboard: it has a HID
/// boot keyboard interface (03:01:01).
pub fn is_keyboard(rule: &str) -> bool {
    rule.contains("03:01:01")
}

/// Whether an inserted device is rejected.
pub fn rejects(level: Option<Level>, locked: bool, rule: &str) -> bool {
    match level {
        None => false,
        Some(_) if is_keyboard(rule) => false,
        Some(Level::Always) => true,
        Some(Level::LockScreen) => locked,
    }
}

async fn bus() -> Option<Connection> {
    if std::env::var("HELIX_TEST_USBGUARD").is_ok_and(|v| v == "1") {
        return Connection::session().await.ok();
    }
    if std::env::var("HELIX_BACKEND").is_ok_and(|b| b == "drm") {
        return Connection::system().await.ok();
    }
    None
}

/// Follows USBGuard's device insertions for the rest of the session.
pub async fn run(center: crate::notifications::Center) {
    let Some(conn) = bus().await else { return };
    if let Err(err) = follow(&conn, &center).await {
        tracing::info!(%err, "no USB protection (USBGuard)");
    }
}

pub async fn follow(conn: &Connection, center: &crate::notifications::Center) -> zbus::Result<()> {
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(USBGUARD)?.interface(DEVICES)?.member("DevicePresenceChanged")?.build();
    let mut events = zbus::MessageStream::for_match_rule(rule, conn, None).await?;
    tracing::info!("USB protection follows USBGuard");
    while let Some(Ok(message)) = events.next().await {
        let Ok((id, event, _target, device_rule, _attributes)) = message.body().deserialize::<(u32, u32, u32, String, std::collections::HashMap<String, String>)>() else { continue };
        if event != INSERT || !rejects(read(), center.locked(), &device_rule) {
            continue;
        }
        tracing::info!(id, "rejecting a USB device plugged in");
        match conn.call_method(Some(USBGUARD), DEVICES_PATH, Some(DEVICES), "applyDevicePolicy", &(id, REJECT, false)).await {
            Ok(_) => {
                center.notice("USB Protection", "USB device blocked", "A USB device was plugged in while it is not allowed.", "drive-removable-media-symbolic", false);
            }
            Err(err) => tracing::warn!(%err, id, "USBGuard did not reject the device"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        let stick = r#"allow id 0781:5567 serial "x" name "Cruzer" with-interface 08:06:50"#;
        let keyboard = r#"allow id 046d:c31c name "Keyboard" with-interface { 03:01:01 03:00:00 }"#;
        assert!(!rejects(None, true, stick));
        assert!(rejects(Some(Level::LockScreen), true, stick));
        assert!(!rejects(Some(Level::LockScreen), false, stick));
        assert!(rejects(Some(Level::Always), false, stick));
        assert!(!rejects(Some(Level::Always), true, keyboard), "keyboards stay");
    }
}
