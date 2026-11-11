//! Client-side proxy for `org.gnome.SessionManager`, for other Helix
//! components and tests.

use zbus::zvariant::{ObjectPath, OwnedObjectPath};

#[zbus::proxy(
    interface = "org.gnome.SessionManager",
    default_service = "org.gnome.SessionManager",
    default_path = "/org/gnome/SessionManager"
)]
pub trait SessionManager {
    fn setenv(&self, variable: &str, value: &str) -> zbus::Result<()>;
    fn get_locale(&self, category: i32) -> zbus::Result<String>;
    fn initialization_error(&self, message: &str, fatal: bool) -> zbus::Result<()>;
    fn initialized(&self) -> zbus::Result<()>;
    fn register_client(&self, app_id: &str, ignored: &str) -> zbus::Result<OwnedObjectPath>;
    fn unregister_client(&self, client_id: &ObjectPath<'_>) -> zbus::Result<()>;
    fn inhibit(&self, app_id: &str, ignored: u32, reason: &str, flags: u32) -> zbus::Result<u32>;
    fn uninhibit(&self, inhibit_cookie: u32) -> zbus::Result<()>;
    fn is_inhibited(&self, flags: u32) -> zbus::Result<bool>;
    fn get_inhibitors(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    fn shutdown(&self) -> zbus::Result<()>;
    fn reboot(&self) -> zbus::Result<()>;
    fn suspend(&self) -> zbus::Result<()>;
    fn can_reboot_to_firmware_setup(&self) -> zbus::Result<bool>;
    fn logout(&self, mode: u32) -> zbus::Result<()>;
    fn is_session_running(&self) -> zbus::Result<bool>;

    #[zbus(signal)]
    fn client_added(&self, id: ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    fn client_removed(&self, id: ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    fn inhibitor_added(&self, id: ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    fn inhibitor_removed(&self, id: ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    fn session_running(&self) -> zbus::Result<()>;
    #[zbus(signal)]
    fn session_over(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn session_name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn session_class(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn session_is_active(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn session_is_locked(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn inhibited_actions(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn restore_supported(&self) -> zbus::Result<bool>;
}
