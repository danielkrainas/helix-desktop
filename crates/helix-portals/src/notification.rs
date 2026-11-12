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

/// The keys passed on to the shell as they are.
pub const KEYS: &[&str] = &["title", "body", "markup-body", "icon", "priority", "default-action", "default-action-target", "buttons"];

/// The categories version 2 notifications may have (`SupportedOptions`).
pub const CATEGORIES: &[&str] = &[
    "im.received",
    "alarm.ringing",
    "call.incoming",
    "call.ongoing",
    "call.unanswered",
    "weather.warning.extreme",
    "cellbroadcast.danger.extreme",
    "cellbroadcast.danger.severe",
    "cellbroadcast.amber-alert",
    "cellbroadcast.test",
    "os.battery.low",
    "browser.web-notification",
];

/// Categories shown as urgent (they stay until dealt with).
fn urgent_category(category: &str) -> bool {
    matches!(category, "alarm.ringing" | "call.incoming" | "weather.warning.extreme" | "cellbroadcast.danger.extreme" | "cellbroadcast.amber-alert" | "os.battery.low")
}

fn string(v: &OwnedValue) -> Option<String> {
    let inner = match &**v {
        Value::Value(inner) => (**inner).try_to_owned().ok()?,
        _ => v.try_clone().ok()?,
    };
    String::try_from(inner).ok()
}

fn strings(v: &OwnedValue) -> Vec<String> {
    v.try_clone().ok().and_then(|v| Vec::<String>::try_from(v).ok()).unwrap_or_default()
}

/// The bytes behind a `("file-descriptor", h)` value.
fn fd_bytes(v: &OwnedValue) -> Option<Vec<u8>> {
    let Value::Structure(s) = &**v else { return None };
    let [Value::Str(kind), payload] = s.fields() else { return None };
    if kind.as_str() != "file-descriptor" {
        return None;
    }
    let payload = match payload {
        Value::Value(inner) => &**inner,
        other => other,
    };
    let Value::Fd(fd) = payload else { return None };
    let owned = std::os::fd::AsFd::as_fd(fd).try_clone_to_owned().ok()?;
    let mut file = std::fs::File::from(owned);
    use std::io::{Read, Seek};
    let _ = file.seek(std::io::SeekFrom::Start(0));
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// Where sounds handed over as file descriptors are kept to be played.
fn sound_file(app_id: &str, id: &str, bytes: &[u8]) -> Option<String> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from)?.join("helix-notification-sounds");
    std::fs::create_dir_all(&dir).ok()?;
    let safe = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect::<String>();
    let path = dir.join(format!("{}-{}", safe(app_id), safe(id)));
    std::fs::write(&path, bytes).ok()?;
    Some(path.to_string_lossy().into_owned())
}

/// What the shell gets for a (version 1 or 2) notification: the keys it
/// takes, an icon given as a file descriptor as bytes, and Helix's own
/// keys for the rest: `x-helix-sound` (a sound name or file),
/// `x-helix-transient`, and `priority` from the category.
pub fn forwarded_v2(app_id: &str, id: &str, notification: HashMap<String, OwnedValue>) -> HashMap<String, OwnedValue> {
    let mut out: HashMap<String, OwnedValue> = HashMap::new();
    let o = |v: Value<'_>| OwnedValue::try_from(v).expect("plain value");
    for (k, v) in &notification {
        if !KEYS.contains(&k.as_str()) {
            continue;
        }
        if k == "icon" {
            if let Some(bytes) = fd_bytes(v) {
                out.insert(k.clone(), o(Value::from(("bytes", Value::from(bytes)))));
                continue;
            }
        }
        if let Ok(v) = v.try_clone() {
            out.insert(k.clone(), v);
        }
    }
    let category = notification.get("category").and_then(string).unwrap_or_default();
    if urgent_category(&category) && !notification.contains_key("priority") {
        out.insert("priority".into(), o(Value::from("urgent")));
    }
    let hints: Vec<String> = notification.get("display-hint").map(strings).unwrap_or_default();
    if hints.iter().any(|h| h == "transient") {
        out.insert("x-helix-transient".into(), o(Value::from(true)));
    }
    if let Some(sound) = notification.get("sound") {
        let chosen = match string(sound).as_deref() {
            Some("silent") => None,
            Some("default") => Some("message-new-instant".to_owned()),
            Some(_) => None,
            None => fd_bytes(sound).and_then(|b| sound_file(app_id, id, &b)),
        };
        if let Some(sound) = chosen {
            out.insert("x-helix-sound".into(), o(Value::from(sound)));
        }
    }
    out
}

/// `notification` with only the keys the shell takes (version 1 form).
pub fn forwarded(notification: HashMap<String, OwnedValue>) -> HashMap<String, OwnedValue> {
    forwarded_v2("", "", notification)
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
            .call_method(Some(GTK), GTK_PATH, Some(GTK), "AddNotification", &(app_id.as_str(), id.as_str(), forwarded_v2(&app_id, &id, notification)))
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
        2
    }

    /// The categories and button purposes version 2 may use.
    #[zbus(property, name = "SupportedOptions")]
    fn supported_options(&self) -> HashMap<String, OwnedValue> {
        let list: Vec<&str> = CATEGORIES.to_vec();
        let purposes: Vec<&str> = BUTTON_PURPOSES.to_vec();
        HashMap::from([
            ("category".to_owned(), OwnedValue::try_from(Value::from(list)).expect("array")),
            ("button-purpose".to_owned(), OwnedValue::try_from(Value::from(purposes)).expect("array")),
        ])
    }

    #[zbus(signal)]
    pub async fn action_invoked(emitter: &SignalEmitter<'_>, app_id: &str, id: &str, action: &str, parameter: Vec<Value<'_>>) -> zbus::Result<()>;
}

/// The button purposes the shell understands (as its `BUTTON_PURPOSES`).
pub const BUTTON_PURPOSES: &[&str] = &["im.reply-with-text", "call.accept", "call.decline", "call.hang-up", "call.enable-speakerphone", "call.disable-speakerphone"];

/// The shell's platform-data key carrying a reply.
const RESPONSE_KEY: &str = "x-helix-response";

/// The portal's `ActionInvoked` parameter from the shell's: the target (if
/// any), the platform data, then the reply (if any). The shell sends the
/// target then the reply, and the reply again in its platform data.
pub fn portal_parameter(mut parameter: Vec<OwnedValue>, mut platform: HashMap<String, OwnedValue>) -> Vec<OwnedValue> {
    let response = platform.remove(RESPONSE_KEY);
    if response.is_some() {
        parameter.pop();
    }
    parameter.push(OwnedValue::try_from(Value::from(platform)).expect("dict"));
    parameter.extend(response);
    parameter
}

/// Pass the shell's `ActionInvoked` for this backend's notifications on.
pub async fn follow_actions(conn: Connection, path: &'static str, added: Added) {
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(GTK).and_then(|b| b.member("ActionInvoked")).map(|b| b.build());
    let Ok(rule) = rule else { return };
    let Ok(mut stream) = zbus::MessageStream::for_match_rule(rule, &conn, Some(32)).await else { return };
    while let Some(Ok(message)) = stream.next().await {
        let body = message.body();
        let Ok((app_id, id, action, parameter, platform)) = body.deserialize::<(String, String, String, Vec<OwnedValue>, HashMap<String, OwnedValue>)>() else { continue };
        let ours = added.lock().unwrap_or_else(|e| e.into_inner()).remove(&(app_id.clone(), id.clone()));
        if !ours {
            continue;
        }
        tracing::info!(app_id, id, action, "portal notification action");
        if let Ok(emitter) = SignalEmitter::new(&conn, path) {
            let parameter: Vec<Value<'_>> = portal_parameter(parameter, platform).into_iter().map(Value::from).collect();
            let _ = NotificationBackend::action_invoked(&emitter, &app_id, &id, &action, parameter).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters() {
        let o = |v: Value<'_>| OwnedValue::try_from(v).unwrap();
        // A target and no reply: target, platform data.
        let p = portal_parameter(vec![o(Value::from(7i32))], HashMap::new());
        assert_eq!(p.len(), 2);
        assert_eq!(i32::try_from(p[0].clone()).unwrap(), 7);
        assert!(HashMap::<String, OwnedValue>::try_from(p[1].clone()).unwrap().is_empty());
        // A reply: target, platform data (without Helix's key), reply.
        let platform = HashMap::from([(RESPONSE_KEY.to_owned(), o(Value::from("on my way")))]);
        let p = portal_parameter(vec![o(Value::from("chat-3")), o(Value::from("on my way"))], platform);
        assert_eq!(p.len(), 3);
        assert_eq!(String::try_from(p[0].clone()).unwrap(), "chat-3");
        assert!(HashMap::<String, OwnedValue>::try_from(p[1].clone()).unwrap().is_empty());
        assert_eq!(String::try_from(p[2].clone()).unwrap(), "on my way");
        // No target, a reply.
        let platform = HashMap::from([(RESPONSE_KEY.to_owned(), o(Value::from("ok")))]);
        let p = portal_parameter(vec![o(Value::from("ok"))], platform);
        assert_eq!(p.len(), 2);
        assert_eq!(String::try_from(p[1].clone()).unwrap(), "ok");
    }

    #[test]
    fn keys() {
        let v = |s: &str| OwnedValue::try_from(Value::from(s)).unwrap();
        let n = HashMap::from([("title".to_owned(), v("Hi")), ("unknown".to_owned(), v("x")), ("category".to_owned(), v("im.received"))]);
        let f = forwarded(n);
        assert_eq!(f.len(), 1, "only the title: unknown keys dropped, im.received stays normal");
        assert!(f.contains_key("title"));
    }

    #[test]
    fn version_two() {
        let v = |s: &str| OwnedValue::try_from(Value::from(s)).unwrap();
        let hints = OwnedValue::try_from(Value::from(vec!["transient", "tray"])).unwrap();
        let n = HashMap::from([
            ("title".to_owned(), v("Call")),
            ("category".to_owned(), v("call.incoming")),
            ("sound".to_owned(), v("default")),
            ("display-hint".to_owned(), hints),
        ]);
        let f = forwarded_v2("org.example.Phone", "c1", n);
        let s = |k: &str| String::try_from(f[k].try_clone().unwrap()).unwrap();
        assert_eq!((s("priority").as_str(), s("x-helix-sound").as_str()), ("urgent", "message-new-instant"));
        assert!(bool::try_from(f["x-helix-transient"].try_clone().unwrap()).unwrap());
        let silent = forwarded_v2("a", "b", HashMap::from([("sound".to_owned(), v("silent")), ("priority".to_owned(), v("low")), ("category".to_owned(), v("alarm.ringing"))]));
        assert!(!silent.contains_key("x-helix-sound"));
        assert_eq!(String::try_from(silent["priority"].try_clone().unwrap()).unwrap(), "low", "the app's own priority wins");
    }

    #[test]
    fn icon_from_fd() {
        let path = std::env::temp_dir().join(format!("helix-icon-{}", std::process::id()));
        std::fs::write(&path, b"PNGDATA").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let fd = zbus::zvariant::OwnedFd::from(std::os::fd::OwnedFd::from(file));
        let icon = OwnedValue::try_from(Value::from(("file-descriptor", Value::from(zbus::zvariant::Fd::from(fd))))).unwrap();
        let f = forwarded_v2("a", "b", HashMap::from([("icon".to_owned(), icon)]));
        // A serialized GIcon: (sv), the bytes in the variant.
        let (kind, payload) = <(String, OwnedValue)>::try_from(f["icon"].try_clone().unwrap()).unwrap();
        let payload = match &*payload {
            Value::Value(inner) => OwnedValue::try_from((**inner).try_clone().unwrap()).unwrap(),
            _ => payload,
        };
        assert_eq!((kind.as_str(), Vec::<u8>::try_from(payload).unwrap().as_slice()), ("bytes", b"PNGDATA".as_slice()));
        std::fs::remove_file(&path).unwrap();
    }
}
