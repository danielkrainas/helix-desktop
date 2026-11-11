//! `org.freedesktop.impl.portal.ScreenCast` backend.
//!
//! Contract: `helix-desktop-spec/contracts/dbus/org.freedesktop.impl.portal.ScreenCast.xml`
//! and `org.freedesktop.impl.portal.md` ("ScreenCast").
//!
//! The user picks a monitor in the shell (`org.helix.ScreenCastChooser`);
//! the compositor records it (`org.gnome.Mutter.ScreenCast`) into a
//! PipeWire stream, whose node id goes back to the app.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::StreamExt;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{fdo, interface, Connection, ObjectServer};

use crate::screenshot::{response, with_request};

pub const SOURCE_MONITOR: u32 = 1;
pub const CURSOR_HIDDEN: u32 = 1;
pub const CURSOR_EMBEDDED: u32 = 2;

const MUTTER: &str = "org.gnome.Mutter.ScreenCast";
const MUTTER_PATH: &str = "/org/gnome/Mutter/ScreenCast";
const SHELL: &str = "org.gnome.Shell";
const CHOOSER_PATH: &str = "/org/helix/ScreenCastChooser";
const DISPLAY_CONFIG: &str = "org.gnome.Mutter.DisplayConfig";

type Results = HashMap<String, OwnedValue>;

#[derive(Debug, Default)]
struct Cast {
    app_id: String,
    cursor_mode: u32,
    /// The compositor's session, once started.
    mutter: Option<OwnedObjectPath>,
}

#[derive(Debug, Default)]
pub struct Casts {
    sessions: HashMap<OwnedObjectPath, Cast>,
}

pub type Shared = Arc<Mutex<Casts>>;

fn lock(shared: &Shared) -> MutexGuard<'_, Casts> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

fn owned(v: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(v).expect("plain values")
}

/// The portal Session object of one screen cast.
pub struct CastSession {
    shared: Shared,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl CastSession {
    async fn close(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<()> {
        if let Some(path) = header.path() {
            end(conn, &self.shared, &OwnedObjectPath::from(path.to_owned()), false).await;
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

/// End a portal session: stop the recording, drop the object; emit
/// `Closed` when the end did not come from the frontend.
async fn end(conn: &Connection, shared: &Shared, path: &OwnedObjectPath, notify: bool) {
    let Some(cast) = lock(shared).sessions.remove(path) else { return };
    if let Some(mutter) = &cast.mutter {
        let _ = conn
            .call_method(Some(MUTTER), mutter.as_str(), Some("org.gnome.Mutter.ScreenCast.Session"), "Stop", &())
            .await;
    }
    if notify {
        if let Ok(emitter) = SignalEmitter::new(conn, path.as_str()) {
            let _ = CastSession::closed(&emitter).await;
        }
    }
    let _ = conn.object_server().remove::<CastSession, _>(path.as_str()).await;
    tracing::info!(%path, app_id = cast.app_id, "screen cast ended");
}

/// Monitors to offer: (connector, display name), from DisplayConfig.
async fn monitors(conn: &Connection) -> zbus::Result<Vec<(String, String)>> {
    type Spec = (String, String, String, String);
    type Mode = (String, i32, i32, f64, f64, Vec<f64>, HashMap<String, OwnedValue>);
    type Logical = (i32, i32, f64, u32, bool, Vec<Spec>, HashMap<String, OwnedValue>);
    let reply = conn
        .call_method(Some(DISPLAY_CONFIG), "/org/gnome/Mutter/DisplayConfig", Some(DISPLAY_CONFIG), "GetCurrentState", &())
        .await?;
    type Props = HashMap<String, OwnedValue>;
    type State = (u32, Vec<(Spec, Vec<Mode>, Props)>, Vec<Logical>, Props);
    let (_, monitors, logical, _): State = reply.body().deserialize()?;
    let shown: Vec<String> = logical.iter().flat_map(|l| l.5.iter().map(|s| s.0.clone())).collect();
    Ok(monitors
        .into_iter()
        .filter(|(spec, _, _)| shown.contains(&spec.0))
        .map(|(spec, _, props)| {
            let name = props
                .get("display-name")
                .and_then(|v| String::try_from(v.clone()).ok())
                .unwrap_or_else(|| spec.0.clone());
            (spec.0, name)
        })
        .collect())
}

async fn start(conn: &Connection, shared: &Shared, session: &OwnedObjectPath) -> (u32, Results) {
    let (app_id, cursor_mode) = match lock(shared).sessions.get(session) {
        Some(cast) => (cast.app_id.clone(), cast.cursor_mode),
        None => return (response::OTHER, Results::new()),
    };
    let offered = match monitors(conn).await {
        Ok(m) if !m.is_empty() => m,
        other => {
            tracing::warn!(?other, "no monitors to share");
            return (response::OTHER, Results::new());
        }
    };
    let choice = conn
        .call_method(Some(SHELL), CHOOSER_PATH, Some("org.helix.ScreenCastChooser"), "ChooseMonitor", &(app_id.as_str(), &offered))
        .await
        .and_then(|r| r.body().deserialize::<(bool, String)>());
    let connector = match choice {
        Ok((true, connector)) => connector,
        Ok((false, _)) => return (response::CANCELLED, Results::new()),
        Err(err) => {
            tracing::warn!(%err, "cannot ask which monitor to share");
            return (response::OTHER, Results::new());
        }
    };
    match record(conn, shared, session, &connector, cursor_mode).await {
        Ok(stream) => {
            let results = HashMap::from([
                ("streams".to_owned(), owned(Value::from(vec![stream]))),
                ("persist_mode".to_owned(), owned(Value::from(0u32))),
            ]);
            (response::SUCCESS, results)
        }
        Err(err) => {
            tracing::warn!(%err, connector, "cannot start the screen cast");
            end(conn, shared, session, true).await;
            (response::OTHER, Results::new())
        }
    }
}

/// Record `connector` through the compositor; the stream for the app.
async fn record(
    conn: &Connection,
    shared: &Shared,
    session: &OwnedObjectPath,
    connector: &str,
    cursor_mode: u32,
) -> zbus::Result<(u32, HashMap<String, OwnedValue>)> {
    let reply = conn
        .call_method(Some(MUTTER), MUTTER_PATH, Some("org.gnome.Mutter.ScreenCast"), "CreateSession", &(HashMap::<&str, Value<'_>>::new(),))
        .await?;
    let mutter: OwnedObjectPath = reply.body().deserialize()?;
    if let Some(cast) = lock(shared).sessions.get_mut(session) {
        cast.mutter = Some(mutter.clone());
    }
    let mutter_cursor = if cursor_mode == CURSOR_EMBEDDED { 1u32 } else { 0 };
    let props = HashMap::from([("cursor-mode", Value::from(mutter_cursor))]);
    let reply = conn
        .call_method(Some(MUTTER), mutter.as_str(), Some("org.gnome.Mutter.ScreenCast.Session"), "RecordMonitor", &(connector, props))
        .await?;
    let stream: OwnedObjectPath = reply.body().deserialize()?;
    let parameters: HashMap<String, OwnedValue> = zbus::Proxy::new(conn, MUTTER, stream.as_str(), "org.gnome.Mutter.ScreenCast.Stream")
        .await?
        .get_property("Parameters")
        .await?;
    // Watch for the node id and for the compositor ending the session.
    let added = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.gnome.Mutter.ScreenCast.Stream")?
        .member("PipeWireStreamAdded")?
        .path(stream.clone())?
        .build();
    let mut added = zbus::MessageStream::for_match_rule(added, conn, None).await?;
    let closed = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.gnome.Mutter.ScreenCast.Session")?
        .member("Closed")?
        .path(mutter.clone())?
        .build();
    let mut closed = zbus::MessageStream::for_match_rule(closed, conn, None).await?;
    conn.call_method(Some(MUTTER), mutter.as_str(), Some("org.gnome.Mutter.ScreenCast.Session"), "Start", &())
        .await?;
    let node: u32 = match tokio::time::timeout(Duration::from_secs(10), added.next()).await {
        Ok(Some(Ok(message))) => message.body().deserialize()?,
        _ => return Err(zbus::Error::Failure("no PipeWire stream".into())),
    };
    {
        let (conn, shared, session) = (conn.clone(), shared.clone(), session.clone());
        tokio::spawn(async move {
            if closed.next().await.is_some() {
                // Already gone if the frontend closed it.
                if let Some(cast) = lock(&shared).sessions.get_mut(&session) {
                    cast.mutter = None;
                }
                end(&conn, &shared, &session, true).await;
            }
        });
    }
    let mut props = HashMap::from([("source_type".to_owned(), owned(Value::from(SOURCE_MONITOR)))]);
    for key in ["position", "size"] {
        if let Some(value) = parameters.get(key) {
            props.insert(key.to_owned(), value.try_clone().expect("plain values"));
        }
    }
    tracing::info!(node, connector, "screen cast stream ready");
    Ok((node, props))
}

/// The exported ScreenCast backend.
pub struct ScreenCastBackend {
    pub shared: Shared,
}

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCastBackend {
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
            lock(&shared).sessions.insert(session.clone(), Cast { app_id: app_id.to_owned(), cursor_mode: CURSOR_HIDDEN, mutter: None });
            if !server.at(&session, CastSession { shared: shared.clone() }).await.unwrap_or(false) {
                lock(&shared).sessions.remove(&session);
                return (response::OTHER, Results::new());
            }
            tracing::info!(%session, app_id, "screen cast session");
            (response::SUCCESS, HashMap::from([("session_id".to_owned(), owned(Value::from(session.as_str())))]))
        })
        .await)
    }

    async fn select_sources(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: &str,
        options: HashMap<String, Value<'_>>,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<(u32, Results)> {
        let session = OwnedObjectPath::from(session_handle.into_owned());
        let types = match options.get("types") {
            Some(Value::U32(t)) => *t,
            _ => SOURCE_MONITOR,
        };
        let cursor_mode = match options.get("cursor_mode") {
            Some(Value::U32(m)) => *m,
            _ => CURSOR_HIDDEN,
        };
        let shared = self.shared.clone();
        Ok(with_request(server, handle, || async move {
            if !lock(&shared).sessions.contains_key(&session) {
                return (response::OTHER, Results::new());
            }
            if cursor_mode != CURSOR_HIDDEN && cursor_mode != CURSOR_EMBEDDED {
                end(conn, &shared, &session, true).await;
                return (response::OTHER, Results::new());
            }
            if types & SOURCE_MONITOR == 0 {
                return (response::OTHER, Results::new());
            }
            if let Some(cast) = lock(&shared).sessions.get_mut(&session) {
                cast.cursor_mode = cursor_mode;
            }
            (response::SUCCESS, Results::new())
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
        Ok(with_request(server, handle, || async move { start(conn, &shared, &session).await }).await)
    }

    #[zbus(property)]
    async fn available_source_types(&self) -> u32 {
        SOURCE_MONITOR
    }

    #[zbus(property)]
    async fn available_cursor_modes(&self) -> u32 {
        CURSOR_HIDDEN | CURSOR_EMBEDDED
    }

    #[zbus(property, name = "version")]
    async fn version(&self) -> u32 {
        4
    }
}
