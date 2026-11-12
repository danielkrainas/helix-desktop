//! `org.freedesktop.impl.portal.Screenshot` backend.
//!
//! Contract: `helix-desktop-spec/contracts/dbus/org.freedesktop.impl.portal.Screenshot.xml`
//! and `org.freedesktop.impl.portal.md` ("Screenshot").
//!
//! Captures go through the shell's `org.gnome.Shell.Screenshot`, which
//! lets this backend's bus name in. Unless the frontend says the app's
//! permission was checked (or the app asks for it), the user picks what to
//! capture in the shell's screenshot UI.

use std::collections::HashMap;
use std::path::PathBuf;

use gio::prelude::*;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{fdo, interface, Connection, ObjectServer};

/// `response` values.
pub mod response {
    pub const SUCCESS: u32 = 0;
    pub const CANCELLED: u32 = 1;
    pub const OTHER: u32 = 2;
}

/// `target` values.
pub mod target {
    pub const SCREEN: u32 = 1;
    pub const WINDOW: u32 = 2;
    pub const AREA: u32 = 4;
    pub const ACTIVE_WINDOW: u32 = 8;
}

/// Targets offered: screen, a window the user picks, area, active window.
pub const AVAILABLE_TARGETS: u32 = target::SCREEN | target::WINDOW | target::AREA | target::ACTIVE_WINDOW;

const SHELL: &str = "org.gnome.Shell";
const SHELL_PATH: &str = "/org/gnome/Shell/Screenshot";
const SHELL_IFACE: &str = "org.gnome.Shell.Screenshot";
/// The shell's message when the user cancels.
const CANCELLED: &str = "Operation was cancelled";

pub(crate) type Results = HashMap<String, OwnedValue>;

/// What to do for a `Screenshot` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Let the user decide in the screenshot UI.
    Ask,
    Screen,
    Area,
    ActiveWindow,
    /// A window the user picks.
    Window,
    Unavailable,
}

pub fn plan(options: &HashMap<String, Value<'_>>) -> Plan {
    let flag = |key: &str| matches!(options.get(key), Some(Value::Bool(true)));
    if flag("interactive") || !flag("permission_store_checked") {
        return Plan::Ask;
    }
    match options.get("target") {
        None | Some(Value::U32(target::SCREEN)) => Plan::Screen,
        Some(Value::U32(target::AREA)) => Plan::Area,
        Some(Value::U32(target::ACTIVE_WINDOW)) => Plan::ActiveWindow,
        Some(Value::U32(target::WINDOW)) => Plan::Window,
        _ => Plan::Unavailable,
    }
}

/// `Screenshot From YYYY-MM-DD HH-MM-SS.png` in the Screenshots folder of
/// the pictures directory, as the shell names its own.
fn screenshot_path() -> PathBuf {
    let pictures = gio::glib::user_special_dir(gio::glib::UserDirectory::Pictures)
        .filter(|d| d.is_dir())
        .unwrap_or_else(gio::glib::home_dir);
    let dir = pictures.join("Screenshots");
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!(%err, "cannot create the Screenshots folder");
    }
    let stamp = gio::glib::DateTime::now_local()
        .and_then(|now| now.format("%Y-%m-%d %H-%M-%S"))
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "unknown time".into());
    dir.join(format!("Screenshot From {stamp}.png"))
}

fn is_cancel(err: &zbus::Error) -> bool {
    matches!(err, zbus::Error::MethodError(_, Some(message), _) if message == CANCELLED)
}

async fn shell<B, R>(conn: &Connection, method: &str, body: &B) -> zbus::Result<R>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    let reply = conn.call_method(Some(SHELL), SHELL_PATH, Some(SHELL_IFACE), method, body).await?;
    reply.body().deserialize::<R>()
}

fn uri_result(uri: String) -> (u32, Results) {
    match OwnedValue::try_from(Value::from(uri)) {
        Ok(v) => (response::SUCCESS, HashMap::from([("uri".to_owned(), v)])),
        Err(_) => (response::OTHER, Results::new()),
    }
}

/// A capture saved by the shell at `path`: `(success, filename_used)`.
fn saved(result: zbus::Result<(bool, String)>) -> (u32, Results) {
    match result {
        Ok((true, used)) => uri_result(gio::File::for_path(used).uri().to_string()),
        Ok((false, _)) => (response::OTHER, Results::new()),
        Err(err) if is_cancel(&err) => (response::CANCELLED, Results::new()),
        Err(err) => {
            tracing::warn!(%err, "shell screenshot failed");
            (response::OTHER, Results::new())
        }
    }
}

async fn take(conn: &Connection, plan: Plan) -> (u32, Results) {
    let path = || screenshot_path().to_string_lossy().into_owned();
    match plan {
        Plan::Ask => match shell::<_, (bool, String)>(conn, "InteractiveScreenshot", &()).await {
            Ok((true, uri)) => uri_result(uri),
            Ok((false, _)) => (response::CANCELLED, Results::new()),
            Err(err) => {
                tracing::warn!(%err, "shell screenshot UI failed");
                (response::OTHER, Results::new())
            }
        },
        Plan::Screen => saved(shell(conn, "Screenshot", &(false, false, path().as_str())).await),
        Plan::ActiveWindow => saved(shell(conn, "ScreenshotWindow", &(false, false, false, path().as_str())).await),
        Plan::Window => {
            let reply = conn
                .call_method(Some(SHELL), SHELL_PATH, Some("org.helix.ScreenshotWindowPicker"), "PickWindow", &(false, false, path().as_str()))
                .await;
            saved(reply.and_then(|r| r.body().deserialize::<(bool, String)>()))
        }
        Plan::Area => match shell::<_, (i32, i32, i32, i32)>(conn, "SelectArea", &()).await {
            Ok((x, y, w, h)) => saved(shell(conn, "ScreenshotArea", &(x, y, w, h, false, path().as_str())).await),
            Err(err) if is_cancel(&err) => (response::CANCELLED, Results::new()),
            Err(err) => {
                tracing::warn!(%err, "area selection failed");
                (response::OTHER, Results::new())
            }
        },
        Plan::Unavailable => (response::OTHER, Results::new()),
    }
}

/// A Request object for the duration of one call.
pub(crate) struct Request;

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl Request {
    async fn close(&self, #[zbus(object_server)] server: &ObjectServer, #[zbus(header)] header: zbus::message::Header<'_>) {
        // The answer of the call it stands for is discarded by the frontend.
        if let Some(path) = header.path() {
            let _ = server.remove::<Request, _>(path).await;
        }
    }
}

pub(crate) async fn with_request<F, Fut>(server: &ObjectServer, handle: ObjectPath<'_>, work: F) -> (u32, Results)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = (u32, Results)>,
{
    let path = OwnedObjectPath::from(handle.into_owned());
    let exported = server.at(&path, Request).await.unwrap_or(false);
    let result = work().await;
    if exported {
        let _ = server.remove::<Request, _>(&path).await;
    }
    result
}

/// The exported Screenshot backend.
pub struct ScreenshotBackend;

#[interface(name = "org.freedesktop.impl.portal.Screenshot")]
impl ScreenshotBackend {
    async fn screenshot(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        _parent_window: &str,
        options: HashMap<String, Value<'_>>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<(u32, Results)> {
        let plan = plan(&options);
        tracing::info!(app_id, ?plan, "screenshot requested");
        Ok(with_request(server, handle, || take(conn, plan)).await)
    }

    async fn pick_color(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        _parent_window: &str,
        _options: HashMap<String, Value<'_>>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<(u32, Results)> {
        tracing::info!(app_id, "color pick requested");
        Ok(with_request(server, handle, || async {
            match shell::<_, Results>(conn, "PickColor", &()).await {
                Ok(result) if result.contains_key("color") => (response::SUCCESS, result),
                Ok(_) => (response::OTHER, Results::new()),
                Err(err) if is_cancel(&err) => (response::CANCELLED, Results::new()),
                Err(err) => {
                    tracing::warn!(%err, "shell color picker failed");
                    (response::OTHER, Results::new())
                }
            }
        })
        .await)
    }

    #[zbus(property)]
    async fn available_targets(&self) -> u32 {
        AVAILABLE_TARGETS
    }

    #[zbus(property, name = "version")]
    async fn version(&self) -> u32 {
        3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(entries: &[(&str, Value<'static>)]) -> HashMap<String, Value<'static>> {
        entries.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
    }

    #[test]
    fn asks_unless_permission_was_checked() {
        assert_eq!(plan(&options(&[])), Plan::Ask, "nobody asked the user yet");
        let checked = ("permission_store_checked", Value::from(true));
        assert_eq!(plan(&options(std::slice::from_ref(&checked))), Plan::Screen);
        assert_eq!(plan(&options(&[checked.clone(), ("interactive", Value::from(true))])), Plan::Ask);
        assert_eq!(plan(&options(&[checked.clone(), ("target", Value::from(4u32))])), Plan::Area);
        assert_eq!(plan(&options(&[checked.clone(), ("target", Value::from(8u32))])), Plan::ActiveWindow);
        assert_eq!(plan(&options(&[checked.clone(), ("target", Value::from(1u32))])), Plan::Screen);
        assert_eq!(plan(&options(&[checked.clone(), ("target", Value::from(2u32))])), Plan::Window);
        assert_eq!(plan(&options(&[checked, ("target", Value::from(16u32))])), Plan::Unavailable);
    }
}
