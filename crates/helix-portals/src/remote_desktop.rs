//! The RemoteDesktop portal backend (helix-desktop-spec
//! `org.freedesktop.impl.portal.md`, "RemoteDesktop"): asks the user, then
//! controls the session through `org.gnome.Mutter.RemoteDesktop`. Sessions
//! share the ScreenCast backend's table, so an app can also pick what to
//! share for the same session.

use std::collections::HashMap;

use zbus::zvariant::{ObjectPath, OwnedFd, OwnedObjectPath, OwnedValue, Value};
use zbus::{fdo, interface, Connection, ObjectServer};

use crate::screencast::{self, lock, owned, Cast, CastSession, Shared, REMOTE};
use crate::screenshot::{response, with_request};

type Results = HashMap<String, OwnedValue>;

pub const KEYBOARD: u32 = 1;
pub const POINTER: u32 = 2;
pub const TOUCHSCREEN: u32 = 4;
pub const ALL_DEVICES: u32 = KEYBOARD | POINTER | TOUCHSCREEN;

/// The exported RemoteDesktop backend.
pub struct RemoteDesktopBackend {
    pub shared: Shared,
}

impl RemoteDesktopBackend {
    /// The compositor session of a started session, if `device` is allowed.
    fn remote_for(&self, session: &ObjectPath<'_>, device: u32) -> Option<(OwnedObjectPath, HashMap<u32, OwnedObjectPath>)> {
        let casts = lock(&self.shared);
        let remote = casts.sessions.get(&OwnedObjectPath::from(session.to_owned()))?.remote.as_ref()?;
        if remote.granted & device == 0 {
            return None;
        }
        Some((remote.mutter.clone()?, remote.streams.clone()))
    }

    /// Pass input on to the compositor's session (when allowed).
    async fn notify<B>(&self, conn: &Connection, session: &ObjectPath<'_>, device: u32, method: &str, body: &B)
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let Some((remote, _)) = self.remote_for(session, device) else { return };
        if let Err(err) = conn.call_method(Some(REMOTE), remote.as_str(), Some("org.gnome.Mutter.RemoteDesktop.Session"), method, body).await {
            tracing::debug!(%err, method, "remote input refused");
        }
    }

    /// The compositor's stream object for PipeWire node `node`.
    fn stream(&self, session: &ObjectPath<'_>, device: u32, node: u32) -> Option<String> {
        self.remote_for(session, device).and_then(|(_, streams)| streams.get(&node).map(|p| p.to_string()))
    }
}

#[interface(name = "org.freedesktop.impl.portal.RemoteDesktop")]
impl RemoteDesktopBackend {
    async fn create_session(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _options: HashMap<String, Value<'_>>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<(u32, Results)> {
        let session = OwnedObjectPath::from(session_handle.into_owned());
        let shared = self.shared.clone();
        Ok(with_request(server, handle, || async move {
            let cast = Cast::remote(app_id, ALL_DEVICES);
            lock(&shared).sessions.insert(session.clone(), cast);
            if !server.at(&session, CastSession { shared: shared.clone() }).await.unwrap_or(false) {
                lock(&shared).sessions.remove(&session);
                return (response::OTHER, Results::new());
            }
            tracing::info!(%session, app_id, "remote desktop session");
            (response::SUCCESS, HashMap::from([("session_id".to_owned(), owned(Value::from(session.as_str())))]))
        })
        .await)
    }

    async fn select_devices(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: &str,
        options: HashMap<String, Value<'_>>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<(u32, Results)> {
        let session = OwnedObjectPath::from(session_handle.into_owned());
        let types = match options.get("types") {
            Some(Value::U32(t)) => *t & ALL_DEVICES,
            _ => ALL_DEVICES,
        };
        let persist_mode = match options.get("persist_mode") {
            Some(Value::U32(m)) if *m <= 2 => *m,
            _ => 0,
        };
        let restore = options.get("restore_data").and_then(screencast::Remembered::from_restore_data);
        let shared = self.shared.clone();
        Ok(with_request(server, handle, || async move {
            let mut casts = lock(&shared);
            match casts.sessions.get_mut(&session) {
                Some(cast) if types != 0 && cast.remote.is_some() => {
                    if let Some(remote) = cast.remote.as_mut() {
                        remote.devices = types;
                    }
                    cast.persist_mode = persist_mode;
                    cast.restore = restore;
                    (response::SUCCESS, Results::new())
                }
                _ => (response::OTHER, Results::new()),
            }
        })
        .await)
    }

    #[allow(clippy::too_many_arguments)]
    async fn start(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: &str,
        _parent_window: &str,
        _options: HashMap<String, Value<'_>>,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<(u32, Results)> {
        let session = OwnedObjectPath::from(session_handle.into_owned());
        let shared = self.shared.clone();
        Ok(with_request(server, handle, || async move { screencast::start(conn, &shared, &session).await }).await)
    }

    async fn notify_pointer_motion(&self, session_handle: ObjectPath<'_>, _options: HashMap<String, Value<'_>>, dx: f64, dy: f64, #[zbus(connection)] conn: &Connection) {
        self.notify(conn, &session_handle, POINTER, "NotifyPointerMotionRelative", &(dx, dy)).await;
    }

    async fn notify_pointer_motion_absolute(
        &self,
        session_handle: ObjectPath<'_>,
        _options: HashMap<String, Value<'_>>,
        stream: u32,
        x: f64,
        y: f64,
        #[zbus(connection)] conn: &Connection,
    ) {
        if let Some(stream) = self.stream(&session_handle, POINTER, stream) {
            self.notify(conn, &session_handle, POINTER, "NotifyPointerMotionAbsolute", &(stream.as_str(), x, y)).await;
        }
    }

    async fn notify_pointer_button(&self, session_handle: ObjectPath<'_>, _options: HashMap<String, Value<'_>>, button: i32, state: u32, #[zbus(connection)] conn: &Connection) {
        self.notify(conn, &session_handle, POINTER, "NotifyPointerButton", &(button, state != 0)).await;
    }

    async fn notify_pointer_axis(&self, session_handle: ObjectPath<'_>, options: HashMap<String, Value<'_>>, dx: f64, dy: f64, #[zbus(connection)] conn: &Connection) {
        let finish = matches!(options.get("finish"), Some(Value::Bool(true)));
        // Smooth scrolling from the portal: finger scrolling.
        let flags = u32::from(finish) | 4;
        self.notify(conn, &session_handle, POINTER, "NotifyPointerAxis", &(dx, dy, flags)).await;
    }

    async fn notify_pointer_axis_discrete(&self, session_handle: ObjectPath<'_>, _options: HashMap<String, Value<'_>>, axis: u32, steps: i32, #[zbus(connection)] conn: &Connection) {
        self.notify(conn, &session_handle, POINTER, "NotifyPointerAxisDiscrete", &(axis, steps)).await;
    }

    async fn notify_keyboard_keycode(&self, session_handle: ObjectPath<'_>, _options: HashMap<String, Value<'_>>, keycode: i32, state: u32, #[zbus(connection)] conn: &Connection) {
        if let Ok(keycode) = u32::try_from(keycode) {
            self.notify(conn, &session_handle, KEYBOARD, "NotifyKeyboardKeycode", &(keycode, state != 0)).await;
        }
    }

    async fn notify_keyboard_keysym(&self, session_handle: ObjectPath<'_>, _options: HashMap<String, Value<'_>>, keysym: i32, state: u32, #[zbus(connection)] conn: &Connection) {
        if let Ok(keysym) = u32::try_from(keysym) {
            self.notify(conn, &session_handle, KEYBOARD, "NotifyKeyboardKeysym", &(keysym, state != 0)).await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn notify_touch_down(
        &self,
        session_handle: ObjectPath<'_>,
        _options: HashMap<String, Value<'_>>,
        stream: u32,
        slot: u32,
        x: f64,
        y: f64,
        #[zbus(connection)] conn: &Connection,
    ) {
        if let Some(stream) = self.stream(&session_handle, TOUCHSCREEN, stream) {
            self.notify(conn, &session_handle, TOUCHSCREEN, "NotifyTouchDown", &(stream.as_str(), slot, x, y)).await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn notify_touch_motion(
        &self,
        session_handle: ObjectPath<'_>,
        _options: HashMap<String, Value<'_>>,
        stream: u32,
        slot: u32,
        x: f64,
        y: f64,
        #[zbus(connection)] conn: &Connection,
    ) {
        if let Some(stream) = self.stream(&session_handle, TOUCHSCREEN, stream) {
            self.notify(conn, &session_handle, TOUCHSCREEN, "NotifyTouchMotion", &(stream.as_str(), slot, x, y)).await;
        }
    }

    async fn notify_touch_up(&self, session_handle: ObjectPath<'_>, _options: HashMap<String, Value<'_>>, slot: u32, #[zbus(connection)] conn: &Connection) {
        self.notify(conn, &session_handle, TOUCHSCREEN, "NotifyTouchUp", &(slot,)).await;
    }

    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(
        &self,
        session_handle: ObjectPath<'_>,
        _app_id: &str,
        _options: HashMap<String, Value<'_>>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedFd> {
        let (remote, granted) = {
            let casts = lock(&self.shared);
            let remote = casts
                .sessions
                .get(&OwnedObjectPath::from(session_handle.to_owned()))
                .and_then(|c| c.remote.as_ref())
                .ok_or_else(|| fdo::Error::Failed("not a remote desktop session".into()))?;
            (remote.mutter.clone().ok_or_else(|| fdo::Error::Failed("the session is not started".into()))?, remote.granted)
        };
        let options = HashMap::from([("device-types", Value::from(granted))]);
        let reply = conn
            .call_method(Some(REMOTE), remote.as_str(), Some("org.gnome.Mutter.RemoteDesktop.Session"), "ConnectToEIS", &(options,))
            .await
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        reply.body().deserialize::<OwnedFd>().map_err(|e| fdo::Error::Failed(e.to_string()))
    }

    #[zbus(property)]
    async fn available_device_types(&self) -> u32 {
        ALL_DEVICES
    }

    #[zbus(property, name = "version")]
    async fn version(&self) -> u32 {
        2
    }
}
