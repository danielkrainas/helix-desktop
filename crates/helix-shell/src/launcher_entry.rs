//! Counts, progress and urgency apps put on their dock icon (helix-desktop-spec
//! `contracts/dbus/com.canonical.Unity.LauncherEntry.md`): the
//! `com.canonical.Unity.LauncherEntry` `Update` signal, which apps send on
//! the session bus.

use std::collections::HashMap;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::zvariant::OwnedValue;
use zbus::Connection;

use crate::UiEvent;

pub const INTERFACE: &str = "com.canonical.Unity.LauncherEntry";

/// What an app shows on its icon.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Entry {
    pub count: i64,
    pub count_visible: bool,
    /// 0 to 1.
    pub progress: f64,
    pub progress_visible: bool,
    pub urgent: bool,
}

impl Entry {
    /// The count to show, if any.
    pub fn shown_count(&self) -> Option<i64> {
        (self.count_visible && self.count > 0).then_some(self.count)
    }

    /// The progress to show, if any (kept within 0 and 1).
    pub fn shown_progress(&self) -> Option<f64> {
        self.progress_visible.then_some(self.progress.clamp(0.0, 1.0))
    }

    /// Apply an update's properties (others keep their values).
    pub fn apply(&mut self, props: &HashMap<String, OwnedValue>) {
        let get = |k: &str| props.get(k).and_then(|v| v.try_clone().ok());
        if let Some(v) = get("count") {
            self.count = i64::try_from(v.clone()).or_else(|_| i32::try_from(v.clone()).map(i64::from)).or_else(|_| u32::try_from(v).map(i64::from)).unwrap_or(self.count);
        }
        if let Some(b) = get("count-visible").and_then(|v| bool::try_from(v).ok()) {
            self.count_visible = b;
        }
        if let Some(p) = get("progress").and_then(|v| f64::try_from(v).ok()) {
            self.progress = p;
        }
        if let Some(b) = get("progress-visible").and_then(|v| bool::try_from(v).ok()) {
            self.progress_visible = b;
        }
        if let Some(b) = get("urgent").and_then(|v| bool::try_from(v).ok()) {
            self.urgent = b;
        }
    }
}

/// The app id (desktop file id without `.desktop`) an `app_uri`
/// (`application://<id>.desktop`) names.
pub fn app_of(app_uri: &str) -> Option<String> {
    let id = app_uri.strip_prefix("application://")?;
    let id = id.strip_suffix(".desktop").unwrap_or(id);
    (!id.is_empty()).then(|| id.to_owned())
}

/// Follow apps' updates for the rest of the session: the UI gets each
/// app's entry as it changes (`None` once its sender left the bus).
pub async fn follow(conn: Connection, ui: mpsc::UnboundedSender<UiEvent>) -> zbus::Result<()> {
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(INTERFACE)?.member("Update")?.build();
    let mut updates = zbus::MessageStream::for_match_rule(rule, &conn, None).await?;
    let mut owners = zbus::fdo::DBusProxy::new(&conn).await?.receive_name_owner_changed().await?;
    let mut entries: HashMap<String, Entry> = HashMap::new();
    // Sender → the apps it updated.
    let mut senders: HashMap<String, Vec<String>> = HashMap::new();
    loop {
        tokio::select! {
            message = updates.next() => {
                let Some(Ok(message)) = message else { break };
                let Ok((uri, props)) = message.body().deserialize::<(String, HashMap<String, OwnedValue>)>() else { continue };
                let Some(app) = app_of(&uri) else { continue };
                let entry = entries.entry(app.clone()).or_default();
                entry.apply(&props);
                if let Some(sender) = message.header().sender() {
                    let apps = senders.entry(sender.to_string()).or_default();
                    if !apps.contains(&app) {
                        apps.push(app.clone());
                    }
                }
                let _ = ui.send(UiEvent::LauncherEntry(app, Some(entry.clone())));
            }
            change = owners.next() => {
                let Some(change) = change else { break };
                let Ok(args) = change.args() else { continue };
                if args.new_owner().is_some() {
                    continue;
                }
                for app in senders.remove(args.name().as_str()).unwrap_or_default() {
                    entries.remove(&app);
                    let _ = ui.send(UiEvent::LauncherEntry(app, None));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    fn o(v: Value<'_>) -> OwnedValue {
        OwnedValue::try_from(v).unwrap()
    }

    #[test]
    fn updates() {
        assert_eq!(app_of("application://firefox.desktop").as_deref(), Some("firefox"));
        assert_eq!(app_of("application://"), None);
        assert_eq!(app_of("file:///x"), None);
        let mut e = Entry::default();
        e.apply(&HashMap::from([("count".to_owned(), o(Value::from(5i64))), ("count-visible".to_owned(), o(Value::from(true)))]));
        assert_eq!(e.shown_count(), Some(5));
        e.apply(&HashMap::from([("progress".to_owned(), o(Value::from(1.4f64))), ("progress-visible".to_owned(), o(Value::from(true)))]));
        assert_eq!((e.shown_count(), e.shown_progress()), (Some(5), Some(1.0)), "count kept, progress clamped");
        e.apply(&HashMap::from([("count-visible".to_owned(), o(Value::from(false))), ("urgent".to_owned(), o(Value::from(true)))]));
        assert_eq!((e.shown_count(), e.urgent), (None, true));
    }
}
