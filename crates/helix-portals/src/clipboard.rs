//! The Clipboard portal backend (helix-desktop-spec
//! `org.freedesktop.impl.portal.md`, "Clipboard"): remote desktop sessions
//! share the clipboard through their `org.gnome.Mutter.RemoteDesktop`
//! session; input capture sessions through their `org.helix.InputCapture`
//! session.

use std::collections::HashMap;

use futures_util::StreamExt;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedFd, OwnedObjectPath, OwnedValue, Value};
use zbus::{fdo, interface, Connection};

use crate::screencast::{lock, Shared, REMOTE};
use crate::OBJECT_PATH;

const MUTTER_SESSION: &str = "org.gnome.Mutter.RemoteDesktop.Session";

/// The exported Clipboard backend.
pub struct ClipboardBackend {
    pub shared: Shared,
    /// Input capture sessions: started, and made but not started.
    pub captures: crate::input_capture::Sessions,
    pub capture_pending: crate::input_capture::Pending,
}

/// The portal's option names → the compositor's (`mime_types` →
/// `mime-types`, `session_is_owner` → `session-is-owner`) and back.
pub(crate) fn rename(options: HashMap<String, OwnedValue>, to_mutter: bool) -> HashMap<String, OwnedValue> {
    options
        .into_iter()
        .map(|(k, v)| (if to_mutter { k.replace('_', "-") } else { k.replace('-', "_") }, v))
        .collect()
}

impl ClipboardBackend {
    /// The compositor session of `session` (bus name, path, interface),
    /// when it shares the clipboard.
    fn target(&self, session: &ObjectPath<'_>) -> fdo::Result<(&'static str, OwnedObjectPath, &'static str)> {
        let portal = OwnedObjectPath::from(session.to_owned());
        if let Some(found) = crate::input_capture::clipboard_session(&self.captures, &portal) {
            let (bus, iface) = crate::input_capture::CLIPBOARD_TARGET;
            return found.map(|path| (bus, path, iface));
        }
        let casts = lock(&self.shared);
        let remote = casts
            .sessions
            .get(&portal)
            .and_then(|c| c.remote.as_ref())
            .ok_or_else(|| fdo::Error::InvalidArgs("no such remote desktop or input capture session".into()))?;
        match (&remote.mutter, remote.clipboard) {
            (Some(m), true) => Ok((REMOTE, m.clone(), MUTTER_SESSION)),
            _ => Err(fdo::Error::Failed("the clipboard is not shared".into())),
        }
    }

    async fn call<B>(&self, conn: &Connection, session: &ObjectPath<'_>, method: &str, body: &B) -> fdo::Result<zbus::Message>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let (bus, path, iface) = self.target(session)?;
        conn.call_method(Some(bus), path.as_str(), Some(iface), method, body).await.map_err(fdo::Error::from)
    }
}

#[interface(name = "org.freedesktop.impl.portal.Clipboard")]
impl ClipboardBackend {
    async fn request_clipboard(&self, session_handle: ObjectPath<'_>, _options: HashMap<String, Value<'_>>) -> fdo::Result<()> {
        let portal = OwnedObjectPath::from(session_handle.to_owned());
        if let Some(result) = crate::input_capture::request_clipboard(&self.captures, &self.capture_pending, &portal) {
            return result;
        }
        let mut casts = lock(&self.shared);
        let remote = casts
            .sessions
            .get_mut(&OwnedObjectPath::from(session_handle.into_owned()))
            .and_then(|c| c.remote.as_mut())
            .ok_or_else(|| fdo::Error::InvalidArgs("no such remote desktop session".into()))?;
        if remote.mutter.is_some() {
            return Err(fdo::Error::Failed("the session is already started".into()));
        }
        remote.clipboard_requested = true;
        Ok(())
    }

    async fn set_selection(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<()> {
        self.call(conn, &session_handle, "SetSelection", &(rename(options, true),)).await.map(|_| ())
    }

    async fn selection_write(&self, session_handle: ObjectPath<'_>, serial: u32, #[zbus(connection)] conn: &Connection) -> fdo::Result<OwnedFd> {
        let reply = self.call(conn, &session_handle, "SelectionWrite", &(serial,)).await?;
        reply.body().deserialize::<OwnedFd>().map_err(fdo::Error::from)
    }

    async fn selection_write_done(
        &self,
        session_handle: ObjectPath<'_>,
        serial: u32,
        success: bool,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<()> {
        self.call(conn, &session_handle, "SelectionWriteDone", &(serial, success)).await.map(|_| ())
    }

    async fn selection_read(&self, session_handle: ObjectPath<'_>, mime_type: &str, #[zbus(connection)] conn: &Connection) -> fdo::Result<OwnedFd> {
        let reply = self.call(conn, &session_handle, "SelectionRead", &(mime_type,)).await?;
        reply.body().deserialize::<OwnedFd>().map_err(fdo::Error::from)
    }

    #[zbus(signal)]
    pub async fn selection_owner_changed(emitter: &SignalEmitter<'_>, session_handle: ObjectPath<'_>, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn selection_transfer(emitter: &SignalEmitter<'_>, session_handle: ObjectPath<'_>, mime_type: &str, serial: u32) -> zbus::Result<()>;

    #[zbus(property, name = "version")]
    async fn version(&self) -> u32 {
        1
    }
}

/// Turn on clipboard sharing for portal session `session` (compositor
/// session `mutter`, started) and pass its signals on; true when on.
pub(crate) async fn enable(conn: &Connection, shared: &Shared, session: &OwnedObjectPath, mutter: &OwnedObjectPath) -> bool {
    let rule = |member: &'static str| -> zbus::Result<zbus::MatchRule<'static>> {
        Ok(zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(MUTTER_SESSION)?
            .member(member)?
            .path(mutter.clone())?
            .build())
    };
    let streams = async {
        let owner = zbus::MessageStream::for_match_rule(rule("SelectionOwnerChanged")?, conn, None).await?;
        let transfer = zbus::MessageStream::for_match_rule(rule("SelectionTransfer")?, conn, None).await?;
        let closed = zbus::MessageStream::for_match_rule(rule("Closed")?, conn, None).await?;
        zbus::Result::Ok((owner, transfer, closed))
    };
    let (mut owner, mut transfer, mut closed) = match streams.await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(%err, "cannot follow the compositor's clipboard");
            return false;
        }
    };
    let none: HashMap<&str, Value<'_>> = HashMap::new();
    if let Err(err) = conn.call_method(Some(REMOTE), mutter.as_str(), Some(MUTTER_SESSION), "EnableClipboard", &(none,)).await {
        tracing::warn!(%err, "cannot share the clipboard");
        return false;
    }
    if let Some(r) = lock(shared).sessions.get_mut(session).and_then(|c| c.remote.as_mut()) {
        r.clipboard = true;
    }
    let (conn, session) = (conn.clone(), session.clone());
    tokio::spawn(async move {
        let Ok(emitter) = SignalEmitter::new(&conn, OBJECT_PATH) else { return };
        loop {
            tokio::select! {
                Some(Ok(msg)) = owner.next() => {
                    if let Ok((options,)) = msg.body().deserialize::<(HashMap<String, OwnedValue>,)>() {
                        let _ = ClipboardBackend::selection_owner_changed(&emitter, session.as_ref(), rename(options, false)).await;
                    }
                }
                Some(Ok(msg)) = transfer.next() => {
                    if let Ok((mime, serial)) = msg.body().deserialize::<(String, u32)>() {
                        let _ = ClipboardBackend::selection_transfer(&emitter, session.as_ref(), &mime, serial).await;
                    }
                }
                _ = closed.next() => break,
                else => break,
            }
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_names() {
        let o = HashMap::from([("mime_types".to_owned(), OwnedValue::from(1u32)), ("session_is_owner".to_owned(), OwnedValue::from(true))]);
        let m = rename(o, true);
        assert!(m.contains_key("mime-types") && m.contains_key("session-is-owner"));
        let p = rename(m, false);
        assert!(p.contains_key("mime_types") && p.contains_key("session_is_owner"));
    }
}
