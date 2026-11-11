//! Notification servers: `org.freedesktop.Notifications` and
//! `org.gtk.Notifications` (contracts in helix-desktop-spec), sharing one
//! queue of banners shown by the shell UI.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use gio::prelude::*;
use tokio::sync::mpsc;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{fdo, interface, Connection};

use crate::notify_store::{CloseReason, Notification, Origin, Store, Urgency};
use crate::screensaver::{self, SharedStatus};
use crate::UiEvent;

pub const FDO_BUS_NAME: &str = "org.freedesktop.Notifications";
pub const FDO_PATH: &str = "/org/freedesktop/Notifications";
pub const GTK_BUS_NAME: &str = "org.gtk.Notifications";
pub const GTK_PATH: &str = "/org/gtk/Notifications";

/// What the UI draws for the current banner.
#[derive(Debug, Clone, PartialEq)]
pub struct Banner {
    pub id: u32,
    pub app_name: String,
    pub summary: String,
    pub body: String,
    /// Buttons as (key, label).
    pub buttons: Vec<(String, String)>,
}

impl From<&Notification> for Banner {
    fn from(n: &Notification) -> Self {
        Self {
            id: n.id,
            app_name: n.app_name.clone(),
            summary: n.summary.clone(),
            body: n.body.clone(),
            buttons: n.actions.iter().filter(|(k, _)| k != "default").cloned().collect(),
        }
    }
}

/// Shared by both D-Bus objects, the expiry ticker and the UI bridge.
#[derive(Clone)]
pub struct Center {
    store: Arc<Mutex<Store>>,
    /// org.gtk action parameters, by notification id and action key.
    targets: Arc<Mutex<HashMap<(u32, String), OwnedValue>>>,
    ui: mpsc::UnboundedSender<UiEvent>,
    lock: SharedStatus,
    /// `org.gnome.desktop.notifications show-banners` (false = do not disturb).
    pub show_banners: Arc<AtomicBool>,
    shown: Arc<Mutex<Option<u32>>>,
}

impl Center {
    pub fn new(ui: mpsc::UnboundedSender<UiEvent>, lock: SharedStatus) -> Self {
        Self {
            store: Default::default(),
            targets: Default::default(),
            ui,
            lock,
            show_banners: Arc::new(AtomicBool::new(true)),
            shown: Default::default(),
        }
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Tell the UI what to show now (if it changed).
    pub fn refresh(&self) {
        let locked = screensaver::status(&self.lock).locked();
        let banners = self.show_banners.load(Ordering::Relaxed);
        let banner = {
            let mut store = self.store();
            if locked {
                store.requeue_current();
                None
            } else {
                store
                    .current(Instant::now(), |n| banners || n.urgency == Urgency::Critical)
                    .map(Banner::from)
            }
        };
        let mut shown = self.shown.lock().unwrap_or_else(|e| e.into_inner());
        let id = banner.as_ref().map(|b| b.id);
        if *shown != id || banner.is_some() {
            *shown = id;
            let _ = self.ui.send(UiEvent::Banner(banner));
        }
    }

    /// "Screenshot captured", for a screenshot the user took.
    pub fn screenshot_captured(&self, path: std::path::PathBuf) -> u32 {
        self.post(Notification {
            id: 0,
            app_name: "Screenshot".into(),
            summary: "Screenshot captured".into(),
            body: "You can paste the image from the clipboard.".into(),
            actions: vec![("default".into(), "Open".into()), ("show-in-files".into(), "Show in Files".into())],
            has_default: true,
            urgency: Urgency::Normal,
            timeout: Notification::timeout_from(-1, Urgency::Normal),
            origin: Origin::Screenshot { path },
        })
    }

    fn post(&self, notification: Notification) -> u32 {
        let id = self.store().add(notification);
        self.refresh();
        id
    }

    /// Remove a notification; returns it so callers can signal.
    fn remove(&self, id: u32) -> Option<Notification> {
        let n = self.store().close(id);
        self.targets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(nid, _), _| *nid != id);
        if n.is_some() {
            self.refresh();
        }
        n
    }

    /// Close and tell freedesktop clients why.
    pub async fn close(&self, conn: &Connection, id: u32, reason: CloseReason) -> zbus::Result<()> {
        if let Some(n) = self.remove(id) {
            if matches!(n.origin, Origin::Freedesktop { .. }) {
                let emitter = SignalEmitter::new(conn, FDO_PATH)?;
                FdoNotifications::notification_closed(&emitter, id, reason as u32).await?;
            }
        }
        Ok(())
    }

    /// The user clicked the banner (`action` = None means the default
    /// action) or a button (`Some(key)`).
    pub async fn activate(&self, conn: &Connection, id: u32, action: Option<String>) -> zbus::Result<()> {
        let Some(n) = self.store().get(id).cloned() else { return Ok(()) };
        let key = action.unwrap_or_else(|| "default".to_owned());
        match &n.origin {
            Origin::Freedesktop { resident } => {
                if key != "default" || n.has_default {
                    let emitter = SignalEmitter::new(conn, FDO_PATH)?;
                    FdoNotifications::action_invoked(&emitter, id, &key).await?;
                }
                if !resident {
                    self.close(conn, id, CloseReason::Dismissed).await?;
                }
            }
            Origin::Gtk { app_id, default_action, buttons, .. } => {
                let action = if key == "default" {
                    default_action.clone()
                } else {
                    key.strip_prefix("button-")
                        .and_then(|i| i.parse::<usize>().ok())
                        .and_then(|i| buttons.get(i).cloned())
                };
                let target = self
                    .targets
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&(id, key.clone()))
                    .and_then(|v| v.try_clone().ok());
                if let Err(err) = activate_app(conn, app_id, action.as_deref(), target).await {
                    tracing::warn!(%err, app_id, "cannot activate application");
                }
                self.remove(id);
            }
            Origin::Screenshot { path } => {
                if key == "show-in-files" {
                    show_in_files(conn, path).await;
                } else {
                    open_uri(&gio::File::for_path(path).uri());
                }
                self.remove(id);
            }
        }
        Ok(())
    }

    /// Expire what is shown when its time is up.
    pub async fn tick(&self, conn: &Connection) -> zbus::Result<()> {
        let expired = self.store().expired(Instant::now());
        if let Some(id) = expired {
            self.close(conn, id, CloseReason::Expired).await?;
        }
        Ok(())
    }
}

fn open_uri(uri: &str) {
    if let Err(err) = gio::AppInfo::launch_default_for_uri(uri, None::<&gio::AppLaunchContext>) {
        tracing::warn!(%err, uri, "cannot open");
    }
}

/// Show `path` in the file manager, or open its folder.
async fn show_in_files(conn: &Connection, path: &std::path::Path) {
    let uri = gio::File::for_path(path).uri().to_string();
    let shown = conn
        .call_method(
            Some("org.freedesktop.FileManager1"),
            "/org/freedesktop/FileManager1",
            Some("org.freedesktop.FileManager1"),
            "ShowItems",
            &(vec![uri.as_str()], ""),
        )
        .await;
    if let Err(err) = shown {
        tracing::info!(%err, "no file manager service; opening the folder");
        if let Some(dir) = path.parent() {
            open_uri(&gio::File::for_path(dir).uri());
        }
    }
}

/// `org.gnome.Calculator` → `/org/gnome/Calculator`.
pub fn app_object_path(app_id: &str) -> String {
    format!("/{}", app_id.replace('.', "/").replace('-', "_"))
}

/// Activate `app.NAME` (or the app itself) over org.freedesktop.Application.
async fn activate_app(
    conn: &Connection,
    app_id: &str,
    action: Option<&str>,
    target: Option<OwnedValue>,
) -> zbus::Result<()> {
    let path = app_object_path(app_id);
    let platform_data: HashMap<&str, Value<'_>> = HashMap::new();
    match action.and_then(|a| a.strip_prefix("app.")) {
        Some(name) => {
            let parameter: Vec<Value<'_>> = target.map(Value::from).into_iter().collect();
            conn.call_method(
                Some(app_id),
                path.as_str(),
                Some("org.freedesktop.Application"),
                "ActivateAction",
                &(name, parameter, platform_data),
            )
            .await?;
        }
        None => {
            conn.call_method(Some(app_id), path.as_str(), Some("org.freedesktop.Application"), "Activate", &(platform_data,))
                .await?;
        }
    }
    Ok(())
}

fn hint_str(hints: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    match hints.get(key).map(|v| &**v) {
        Some(Value::Str(s)) => Some(s.to_string()),
        _ => None,
    }
}

/// org.freedesktop.Notifications.
pub struct FdoNotifications {
    pub center: Center,
}

#[interface(name = "org.freedesktop.Notifications")]
impl FdoNotifications {
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        _app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let urgency = match hints.get("urgency").map(|v| &**v) {
            Some(Value::U8(0)) => Urgency::Low,
            Some(Value::U8(2)) => Urgency::Critical,
            _ => Urgency::Normal,
        };
        let resident = matches!(hints.get("resident").map(|v| &**v), Some(Value::Bool(true)));
        let pairs: Vec<(String, String)> = actions
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(|c| (c[0].clone(), c[1].clone()))
            .collect();
        let has_default = pairs.iter().any(|(k, _)| k == "default");
        let app_name = if app_name.is_empty() {
            hint_str(&hints, "desktop-entry").unwrap_or_default()
        } else {
            app_name
        };
        let id = self.center.post(Notification {
            id: replaces_id,
            app_name,
            summary,
            body,
            actions: pairs,
            has_default,
            urgency,
            timeout: Notification::timeout_from(expire_timeout, urgency),
            origin: Origin::Freedesktop { resident },
        });
        tracing::debug!(id, "notification");
        id
    }

    async fn close_notification(&self, id: u32, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        Ok(self.center.close(conn, id, CloseReason::Closed).await?)
    }

    async fn get_capabilities(&self) -> Vec<String> {
        // No markup, icons, sounds or persistence yet: clients send plain
        // text and don't rely on the rest.
        vec!["actions".into(), "body".into()]
    }

    #[zbus(out_args("name", "vendor", "version", "spec_version"))]
    async fn get_server_information(&self) -> (String, String, String, String) {
        ("Helix".into(), "Helix".into(), env!("CARGO_PKG_VERSION").into(), "1.2".into())
    }

    #[zbus(signal)]
    pub async fn notification_closed(emitter: &SignalEmitter<'_>, id: u32, reason: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn action_invoked(emitter: &SignalEmitter<'_>, id: u32, action_key: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn activation_token(emitter: &SignalEmitter<'_>, id: u32, activation_token: &str) -> zbus::Result<()>;
}

/// org.gtk.Notifications (GLib's GNotification).
pub struct GtkNotifications {
    pub center: Center,
}

fn dict_str(d: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    match d.get(key).map(|v| &**v) {
        Some(Value::Str(s)) => Some(s.to_string()),
        _ => None,
    }
}

fn unwrap_variant(v: &Value<'_>) -> Option<OwnedValue> {
    let inner = match v {
        Value::Value(inner) => &**inner,
        other => other,
    };
    inner.try_to_owned().ok()
}

#[interface(name = "org.gtk.Notifications")]
impl GtkNotifications {
    async fn add_notification(&self, app_id: String, id: String, notification: HashMap<String, OwnedValue>) {
        let priority = dict_str(&notification, "priority");
        let urgency = match priority.as_deref() {
            Some("low") => Urgency::Low,
            Some("urgent") => Urgency::Critical,
            _ => Urgency::Normal,
        };
        let default_action = dict_str(&notification, "default-action");
        let mut actions = Vec::new();
        let mut button_actions = Vec::new();
        let mut targets = Vec::new();
        if let Some(target) = notification.get("default-action-target").and_then(|v| unwrap_variant(v)) {
            targets.push(("default".to_owned(), target));
        }
        if let Some(Value::Array(buttons)) = notification.get("buttons").map(|v| &**v) {
            for (i, button) in buttons.iter().enumerate() {
                let Ok(owned) = button.try_to_owned() else { continue };
                let Ok(dict) = HashMap::<String, OwnedValue>::try_from(owned) else { continue };
                let label = dict_str(&dict, "label").unwrap_or_default();
                let action = dict_str(&dict, "action").unwrap_or_default();
                let key = format!("button-{i}");
                if let Some(target) = dict.get("target").and_then(|v| unwrap_variant(v)) {
                    targets.push((key.clone(), target));
                }
                actions.push((key, label));
                button_actions.push(action);
            }
        }
        let existing = self.center.store().find_gtk(&app_id, &id).unwrap_or(0);
        let nid = self.center.post(Notification {
            id: existing,
            app_name: app_id.clone(),
            summary: dict_str(&notification, "title").unwrap_or_default(),
            body: dict_str(&notification, "body").unwrap_or_default(),
            actions,
            has_default: true,
            urgency,
            timeout: Notification::timeout_from(-1, urgency),
            origin: Origin::Gtk { app_id, id, default_action, buttons: button_actions },
        });
        let mut map = self.center.targets.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|(n, _), _| *n != nid);
        for (key, target) in targets {
            map.insert((nid, key), target);
        }
    }

    async fn remove_notification(&self, app_id: String, id: String) {
        let found = self.center.store().find_gtk(&app_id, &id);
        if let Some(nid) = found {
            self.center.remove(nid);
        }
    }

    #[zbus(signal)]
    pub async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        app_id: &str,
        id: &str,
        action: &str,
        parameter: Vec<Value<'_>>,
        platform_data: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;
}

/// Follow `org.gnome.desktop.notifications show-banners` (do not disturb).
pub async fn follow_settings(center: Center) {
    const SCHEMA: &str = "org.gnome.desktop.notifications";
    if !helix_settings::has_schema(SCHEMA) {
        return;
    }
    let read = || {
        use gio::prelude::*;
        gio::Settings::new(SCHEMA).boolean("show-banners")
    };
    center.show_banners.store(read(), Ordering::Relaxed);
    center.refresh();
    let settings = helix_settings::GSettings::new(&[SCHEMA]);
    let (mut changes, _watch) = settings.watch();
    while changes.recv().await.is_some() {
        let on = read();
        tracing::info!(show_banners = on, "notification banners setting changed");
        center.show_banners.store(on, Ordering::Relaxed);
        center.refresh();
    }
}

/// Expire banners on time.
pub fn spawn_ticker(center: Center, conn: Connection) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if let Err(err) = center.tick(&conn).await {
                tracing::warn!(%err, "notification expiry failed");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_paths() {
        assert_eq!(app_object_path("org.gnome.Calculator"), "/org/gnome/Calculator");
        assert_eq!(app_object_path("org.example.my-app"), "/org/example/my_app");
    }
}
