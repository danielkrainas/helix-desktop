//! The GlobalShortcuts portal backend (helix-desktop-spec
//! `org.freedesktop.impl.portal.md`, "GlobalShortcuts"): asks the user,
//! then binds the app's shortcuts through the shell's `GrabAccelerators`
//! and passes their presses and releases on.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{fdo, interface, Connection, ObjectServer};

use crate::dialogs::{app_name, dialog, value};
use crate::screenshot::{response, with_request, Results};

const SHELL: &str = "org.gnome.Shell";
const SHELL_PATH: &str = "/org/gnome/Shell";
/// Normal and overview action modes.
const MODES: u32 = 1 | 2;
/// Trigger on release as well.
const TRIGGER_RELEASE: u32 = 128;

/// A shortcut as the app asks for it and as it is bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortcut {
    pub id: String,
    pub description: String,
    /// The accelerator it is bound to (`<Control>a`), if any.
    pub accelerator: Option<String>,
    /// The shell's action id, once grabbed.
    pub action: Option<u32>,
}

/// A session: its app and bound shortcuts.
#[derive(Debug, Default)]
pub struct Session {
    app_id: String,
    shortcuts: Vec<Shortcut>,
}

pub type Sessions = Arc<Mutex<HashMap<OwnedObjectPath, Session>>>;

fn lock(s: &Sessions) -> std::sync::MutexGuard<'_, HashMap<OwnedObjectPath, Session>> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

/// A `preferred_trigger` (`CTRL+SHIFT+a`) as a GTK accelerator
/// (`<Control><Shift>a`).
pub fn accelerator_of(trigger: &str) -> Option<String> {
    let parts: Vec<&str> = trigger.split('+').map(str::trim).collect();
    let (key, modifiers) = parts.split_last()?;
    if key.is_empty() {
        return None;
    }
    let mut out = String::new();
    for m in modifiers {
        match m.to_ascii_uppercase().as_str() {
            "CTRL" | "CONTROL" => out.push_str("<Control>"),
            "ALT" => out.push_str("<Alt>"),
            "SHIFT" => out.push_str("<Shift>"),
            "LOGO" | "SUPER" => out.push_str("<Super>"),
            "NUM" => {}
            _ => return None,
        }
    }
    out.push_str(key);
    Some(out)
}

/// A human-readable form of an accelerator (`Ctrl+Shift+S`).
pub fn trigger_description(accelerator: Option<&str>) -> String {
    let Some(accel) = accelerator else { return "Not assigned".into() };
    let mut rest = accel;
    let mut parts = Vec::new();
    while let Some(after) = rest.strip_prefix('<') {
        let Some(end) = after.find('>') else { break };
        parts.push(match &after[..end] {
            "Control" => "Ctrl",
            "Alt" => "Alt",
            "Shift" => "Shift",
            "Super" => "Super",
            other => other,
        }.to_owned());
        rest = &after[end + 1..];
    }
    let mut key = rest.chars();
    parts.push(key.next().map(|c| c.to_uppercase().chain(key).collect()).unwrap_or_default());
    parts.join("+")
}

/// The shortcuts of a `BindShortcuts` call.
pub fn requested(shortcuts: &[(String, HashMap<String, OwnedValue>)]) -> Vec<Shortcut> {
    shortcuts
        .iter()
        .map(|(id, props)| {
            let get = |k: &str| props.get(k).and_then(|v| String::try_from(v.try_clone().ok()?).ok());
            Shortcut { id: id.clone(), description: get("description").unwrap_or_default(), accelerator: get("preferred_trigger").and_then(|t| accelerator_of(&t)), action: None }
        })
        .collect()
}

/// The results' form of shortcuts.
pub fn described(shortcuts: &[Shortcut]) -> Vec<(String, HashMap<String, OwnedValue>)> {
    shortcuts
        .iter()
        .map(|s| {
            let bound = s.action.and(s.accelerator.as_deref());
            (
                s.id.clone(),
                HashMap::from([
                    ("description".to_owned(), value(Value::from(s.description.as_str()))),
                    ("trigger_description".to_owned(), value(Value::from(trigger_description(bound)))),
                ]),
            )
        })
        .collect()
}

/// Where allowed shortcut sets are remembered.
pub fn store_path() -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    data.join("helix/global-shortcuts")
}

/// The remembered (shortcut id, accelerator) pairs of `app_id`.
pub fn remembered(text: &str, app_id: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let (app, id, accel) = (f.next()?, f.next()?, f.next().unwrap_or(""));
            (app == app_id).then(|| (id.to_owned(), accel.to_owned()))
        })
        .collect()
}

/// `text` with `app_id`'s lines replaced by `shortcuts`.
pub fn remember(text: &str, app_id: &str, shortcuts: &[Shortcut]) -> String {
    let mut out: Vec<String> = text.lines().filter(|l| l.split('\t').next() != Some(app_id)).map(str::to_owned).collect();
    for s in shortcuts {
        out.push(format!("{app_id}\t{}\t{}", s.id, s.accelerator.as_deref().unwrap_or("")));
    }
    out.join("\n") + "\n"
}

/// Whether `shortcuts` were all allowed before, with the same triggers.
pub fn allowed_before(remembered: &[(String, String)], shortcuts: &[Shortcut]) -> bool {
    !shortcuts.is_empty() && shortcuts.iter().all(|s| remembered.iter().any(|(id, accel)| *id == s.id && *accel == s.accelerator.clone().unwrap_or_default()))
}

pub struct GlobalShortcutsBackend {
    pub conn: Connection,
    pub sessions: Sessions,
}

async fn ungrab(conn: &Connection, shortcuts: &[Shortcut]) {
    let actions: Vec<u32> = shortcuts.iter().filter_map(|s| s.action).collect();
    if !actions.is_empty() {
        let _ = conn.call_method(Some(SHELL), SHELL_PATH, Some(SHELL), "UngrabAccelerators", &(actions,)).await;
    }
}

/// Ask (unless allowed before), grab, and keep the session's shortcuts.
async fn bind(conn: &Connection, sessions: &Sessions, session: &OwnedObjectPath, mut shortcuts: Vec<Shortcut>) -> (u32, Results) {
    let Some(app_id) = lock(sessions).get(session).map(|s| s.app_id.clone()) else { return (response::OTHER, Results::new()) };
    let path = store_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if !allowed_before(&remembered(&text, &app_id), &shortcuts) {
        let who = if app_id.is_empty() { "An app".to_owned() } else { app_name(&app_id) };
        let rows: Vec<String> = shortcuts
            .iter()
            .map(|s| match &s.accelerator {
                Some(a) => format!("{} — {}", s.description, trigger_description(Some(a))),
                None => format!("{} — not assigned", s.description),
            })
            .collect();
        let asked = HashMap::from([
            ("title".to_owned(), value(Value::from("Allow Global Shortcuts?"))),
            ("message".to_owned(), value(Value::from(format!("{who} wants to use these shortcuts in every app.")))),
            ("rows".to_owned(), value(Value::from(rows))),
            ("accept".to_owned(), value(Value::from("Allow"))),
            ("cancel".to_owned(), value(Value::from("Deny"))),
        ]);
        if dialog(conn, asked).await.is_none() {
            return (response::CANCELLED, Results::new());
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(err) = std::fs::write(&path, remember(&text, &app_id, &shortcuts)) {
            tracing::warn!(%err, "cannot remember the allowed global shortcuts");
        }
    }
    // The old ones go first.
    let old = lock(sessions).get_mut(session).map(|s| std::mem::take(&mut s.shortcuts)).unwrap_or_default();
    ungrab(conn, &old).await;
    let wanted: Vec<(String, u32, u32)> = shortcuts.iter().filter_map(|s| s.accelerator.clone()).map(|a| (a, MODES, TRIGGER_RELEASE)).collect();
    if !wanted.is_empty() {
        match conn.call_method(Some(SHELL), SHELL_PATH, Some(SHELL), "GrabAccelerators", &(wanted,)).await.and_then(|r| r.body().deserialize::<Vec<u32>>()) {
            Ok(actions) => {
                let mut actions = actions.into_iter();
                for s in shortcuts.iter_mut().filter(|s| s.accelerator.is_some()) {
                    s.action = actions.next().filter(|a| *a != 0);
                }
            }
            Err(err) => tracing::warn!(%err, "the shell cannot grab global shortcuts"),
        }
    }
    tracing::info!(app_id, bound = shortcuts.iter().filter(|s| s.action.is_some()).count(), "global shortcuts bound");
    let list = described(&shortcuts);
    if let Some(s) = lock(sessions).get_mut(session) {
        s.shortcuts = shortcuts;
    }
    if !old.is_empty() {
        if let Ok(emitter) = SignalEmitter::new(conn, crate::OBJECT_PATH) {
            let _ = GlobalShortcutsBackend::shortcuts_changed(&emitter, ObjectPath::from(session), list.clone()).await;
        }
    }
    (response::SUCCESS, HashMap::from([("shortcuts".to_owned(), value(Value::from(list)))]))
}

/// Pass the shell's accelerator signals on as the sessions' shortcuts.
pub async fn follow(conn: Connection, sessions: Sessions) {
    let rule = match zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(SHELL).and_then(|r| r.path(SHELL_PATH)) {
        Ok(rule) => rule.build(),
        Err(_) => return,
    };
    let Ok(mut signals) = zbus::MessageStream::for_match_rule(rule, &conn, None).await else { return };
    let Ok(emitter) = SignalEmitter::new(&conn, crate::OBJECT_PATH) else { return };
    while let Some(Ok(message)) = signals.next().await {
        let member = message.header().member().map(|m| m.to_string()).unwrap_or_default();
        if member != "AcceleratorActivated" && member != "AcceleratorDeactivated" {
            continue;
        }
        let Ok((action, params)) = message.body().deserialize::<(u32, HashMap<String, OwnedValue>)>() else { continue };
        let found = lock(&sessions).iter().find_map(|(path, s)| s.shortcuts.iter().find(|x| x.action == Some(action)).map(|x| (path.clone(), x.id.clone())));
        let Some((session, id)) = found else { continue };
        let time = params.get("timestamp").and_then(|v| u32::try_from(v.try_clone().ok()?).ok()).unwrap_or(0);
        let session = ObjectPath::from(&session);
        let result = if member == "AcceleratorActivated" {
            GlobalShortcutsBackend::activated(&emitter, session, &id, u64::from(time), HashMap::new()).await
        } else {
            GlobalShortcutsBackend::deactivated(&emitter, session, &id, u64::from(time), HashMap::new()).await
        };
        if let Err(err) = result {
            tracing::warn!(%err, "cannot pass a global shortcut on");
        }
    }
}

pub struct ShortcutsSession {
    sessions: Sessions,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl ShortcutsSession {
    async fn close(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection, #[zbus(object_server)] server: &ObjectServer) -> fdo::Result<()> {
        let Some(path) = header.path().map(|p| OwnedObjectPath::from(p.to_owned())) else { return Ok(()) };
        let gone = lock(&self.sessions).remove(&path);
        if let Some(gone) = gone {
            ungrab(conn, &gone.shortcuts).await;
        }
        if server.remove::<ShortcutsSession, _>(path.as_str()).await.unwrap_or(false) {
            if let Ok(emitter) = SignalEmitter::new(conn, path.as_str()) {
                let _ = Self::closed(&emitter).await;
            }
        }
        Ok(())
    }

    #[zbus(signal)]
    pub async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property, name = "version")]
    async fn version(&self) -> u32 {
        1
    }
}

#[interface(name = "org.freedesktop.impl.portal.GlobalShortcuts")]
impl GlobalShortcutsBackend {
    async fn create_session(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        let session = OwnedObjectPath::from(session_handle.into_owned());
        lock(&self.sessions).insert(session.clone(), Session { app_id: app_id.clone(), shortcuts: Vec::new() });
        if !server.at(&session, ShortcutsSession { sessions: self.sessions.clone() }).await.unwrap_or(false) {
            lock(&self.sessions).remove(&session);
            return Ok((response::OTHER, Results::new()));
        }
        tracing::info!(app_id, %session, "global shortcuts session");
        Ok(with_request(server, handle, || async { (response::SUCCESS, Results::new()) }).await)
    }

    async fn bind_shortcuts(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        shortcuts: Vec<(String, HashMap<String, OwnedValue>)>,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        let session = OwnedObjectPath::from(session_handle.into_owned());
        let (conn, sessions) = (self.conn.clone(), self.sessions.clone());
        let shortcuts = requested(&shortcuts);
        Ok(with_request(server, handle, || async move { bind(&conn, &sessions, &session, shortcuts).await }).await)
    }

    async fn list_shortcuts(&self, #[zbus(object_server)] server: &ObjectServer, handle: ObjectPath<'_>, session_handle: ObjectPath<'_>) -> fdo::Result<(u32, Results)> {
        let session = OwnedObjectPath::from(session_handle.into_owned());
        let list = lock(&self.sessions).get(&session).map(|s| described(&s.shortcuts));
        Ok(with_request(server, handle, || async move {
            match list {
                Some(list) => (response::SUCCESS, HashMap::from([("shortcuts".to_owned(), value(Value::from(list)))])),
                None => (response::OTHER, Results::new()),
            }
        })
        .await)
    }

    #[zbus(signal)]
    pub async fn activated(emitter: &SignalEmitter<'_>, session_handle: ObjectPath<'_>, shortcut_id: &str, timestamp: u64, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn deactivated(emitter: &SignalEmitter<'_>, session_handle: ObjectPath<'_>, shortcut_id: &str, timestamp: u64, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn shortcuts_changed(emitter: &SignalEmitter<'_>, session_handle: ObjectPath<'_>, shortcuts: Vec<(String, HashMap<String, OwnedValue>)>) -> zbus::Result<()>;

    #[zbus(property, name = "version")]
    async fn version(&self) -> u32 {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers() {
        assert_eq!(accelerator_of("CTRL+SHIFT+a").as_deref(), Some("<Control><Shift>a"));
        assert_eq!(accelerator_of("LOGO+F5").as_deref(), Some("<Super>F5"));
        assert_eq!(accelerator_of("NUM+ALT+Return").as_deref(), Some("<Alt>Return"));
        assert_eq!(accelerator_of("HYPER+a"), None);
        assert_eq!(accelerator_of(""), None);
        assert_eq!(trigger_description(Some("<Control><Shift>s")), "Ctrl+Shift+S");
        assert_eq!(trigger_description(Some("<Super>F5")), "Super+F5");
        assert_eq!(trigger_description(None), "Not assigned");
    }

    #[test]
    fn remembering() {
        let s = |id: &str, a: Option<&str>| Shortcut { id: id.into(), description: id.into(), accelerator: a.map(String::from), action: None };
        let set = vec![s("talk", Some("<Control>t")), s("mute", None)];
        let text = remember("other\tx\t<Super>x\n", "com.example.Talk", &set);
        let mine = remembered(&text, "com.example.Talk");
        assert_eq!(mine, [("talk".to_owned(), "<Control>t".to_owned()), ("mute".to_owned(), String::new())]);
        assert!(allowed_before(&mine, &set));
        assert!(!allowed_before(&mine, &[s("talk", Some("<Control>y"))]), "another trigger");
        assert!(!allowed_before(&mine, &[s("new", None)]));
        assert_eq!(remembered(&text, "other"), [("x".to_owned(), "<Super>x".to_owned())], "other apps kept");
        let replaced = remember(&text, "com.example.Talk", &[s("talk", Some("<Alt>t"))]);
        assert_eq!(remembered(&replaced, "com.example.Talk").len(), 1);
    }

    #[test]
    fn described_results() {
        let list = described(&[
            Shortcut { id: "a".into(), description: "Talk".into(), accelerator: Some("<Control>t".into()), action: Some(4) },
            Shortcut { id: "b".into(), description: "Mute".into(), accelerator: Some("<Control>m".into()), action: None },
        ]);
        let trig = |i: usize| String::try_from(list[i].1["trigger_description"].try_clone().unwrap()).unwrap();
        assert_eq!((trig(0).as_str(), trig(1).as_str()), ("Ctrl+T", "Not assigned"), "not grabbed: not assigned");
    }
}
