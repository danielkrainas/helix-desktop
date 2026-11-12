//! `org.freedesktop.impl.portal.Notification` (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.impl.portal.md`, "Notification"): apps'
//! notifications go to the shell's `org.gtk.Notifications`, and actions the
//! shell cannot deliver come back as `ActionInvoked`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{fdo, interface, Connection};

const GTK: &str = "org.gtk.Notifications";
const GTK_PATH: &str = "/org/gtk/Notifications";

/// Notifications this backend added: (app id, id).
pub type Added = Arc<Mutex<HashSet<(String, String)>>>;

/// The keys passed on to the shell (version 1 of the interface, and
/// `markup-body`).
pub const KEYS: &[&str] = &["title", "body", "markup-body", "icon", "priority", "default-action", "default-action-target", "buttons"];

/// `notification` with only the keys the shell takes.
pub fn forwarded(notification: HashMap<String, OwnedValue>) -> HashMap<String, OwnedValue> {
    notification.into_iter().filter(|(k, _)| KEYS.contains(&k.as_str())).collect()
}

pub struct NotificationBackend {
    pub conn: Connection,
    pub added: Added,
}

#[interface(name = "org.freedesktop.impl.portal.Notification")]
impl NotificationBackend {
    async fn add_notification(&self, app_id: String, id: String, notification: HashMap<String, OwnedValue>) -> fdo::Result<()> {
        tracing::info!(app_id, id, "portal notification");
        self.conn
            .call_method(Some(GTK), GTK_PATH, Some(GTK), "AddNotification", &(app_id.as_str(), id.as_str(), forwarded(notification)))
            .await
            .map_err(|e| fdo::Error::Failed(format!("the shell cannot show it: {e}")))?;
        self.added.lock().unwrap_or_else(|e| e.into_inner()).insert((app_id, id));
        Ok(())
    }

    async fn remove_notification(&self, app_id: String, id: String) -> fdo::Result<()> {
        self.added.lock().unwrap_or_else(|e| e.into_inner()).remove(&(app_id.clone(), id.clone()));
        self.conn
            .call_method(Some(GTK), GTK_PATH, Some(GTK), "RemoveNotification", &(app_id.as_str(), id.as_str()))
            .await
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        Ok(())
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }

    #[zbus(signal)]
    pub async fn action_invoked(emitter: &SignalEmitter<'_>, app_id: &str, id: &str, action: &str, parameter: Vec<Value<'_>>) -> zbus::Result<()>;
}

/// Pass the shell's `ActionInvoked` for this backend's notifications on.
pub async fn follow_actions(conn: Connection, path: &'static str, added: Added) {
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(GTK).and_then(|b| b.member("ActionInvoked")).map(|b| b.build());
    let Ok(rule) = rule else { return };
    let Ok(mut stream) = zbus::MessageStream::for_match_rule(rule, &conn, Some(32)).await else { return };
    while let Some(Ok(message)) = stream.next().await {
        let body = message.body();
        let Ok((app_id, id, action, parameter, _)) = body.deserialize::<(String, String, String, Vec<OwnedValue>, HashMap<String, OwnedValue>)>() else { continue };
        let ours = added.lock().unwrap_or_else(|e| e.into_inner()).remove(&(app_id.clone(), id.clone()));
        if !ours {
            continue;
        }
        tracing::info!(app_id, id, action, "portal notification action");
        if let Ok(emitter) = SignalEmitter::new(&conn, path) {
            let parameter: Vec<Value<'_>> = parameter.into_iter().map(Value::from).collect();
            let _ = NotificationBackend::action_invoked(&emitter, &app_id, &id, &action, parameter).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        let v = |s: &str| OwnedValue::try_from(Value::from(s)).unwrap();
        let n = HashMap::from([("title".to_owned(), v("Hi")), ("sound".to_owned(), v("default")), ("category".to_owned(), v("im.received"))]);
        let f = forwarded(n);
        assert_eq!(f.len(), 1);
        assert!(f.contains_key("title"));
    }
}
