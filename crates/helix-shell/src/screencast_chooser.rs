//! `org.helix.ScreenCastChooser` (helix-desktop-spec
//! `org.helix.ScreenCastChooser.md`): the portal backend asks which monitor
//! to share; the shell asks the user. Also follows the compositor's
//! `org.helix.ScreenCastState` for the panel's "Sharing" indicator.

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};
use zbus::fdo::{self, DBusProxy};
use zbus::message::Header;
use zbus::{interface, Connection};

use crate::UiEvent;

pub const OBJECT_PATH: &str = "/org/helix/ScreenCastChooser";
const ALLOWED_CALLER: &str = "org.freedesktop.impl.portal.desktop.helix";

/// What the user chose to share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shared {
    Monitor(String),
    /// A window, by its window list id.
    Window(u32),
}

/// A question for the UI: what to share with `app` — one of `monitors`
/// (connector, label), or (with `windows`) an open window.
#[derive(Debug)]
pub struct ShareQuestion {
    pub app: String,
    pub monitors: Vec<(String, String)>,
    pub windows: bool,
    /// The app will be able to share it again without asking.
    pub remember: bool,
    /// Devices the app will also control (a remote desktop session).
    pub control: Vec<String>,
    pub reply: oneshot::Sender<Option<Shared>>,
}

/// A question for the UI: may `app` control `devices` (remote desktop,
/// without screen sharing)?
#[derive(Debug)]
pub struct ControlQuestion {
    pub app: String,
    pub devices: Vec<String>,
    pub reply: oneshot::Sender<bool>,
}

/// A [`ControlQuestion`] that can travel in a [`UiEvent`].
#[derive(Debug, Clone)]
pub struct ControlSlot(Arc<Mutex<Option<ControlQuestion>>>);

impl ControlSlot {
    pub fn new(question: ControlQuestion) -> Self {
        Self(Arc::new(Mutex::new(Some(question))))
    }

    pub fn take(&self) -> Option<ControlQuestion> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl PartialEq for ControlSlot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// "keyboard and pointer", "keyboard, pointer and touchscreen".
pub fn device_list(devices: &[String]) -> String {
    match devices {
        [] => "input".into(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// A [`ShareQuestion`] that can travel in a [`UiEvent`].
#[derive(Debug, Clone)]
pub struct QuestionSlot(Arc<Mutex<Option<ShareQuestion>>>);

impl QuestionSlot {
    pub fn new(question: ShareQuestion) -> Self {
        Self(Arc::new(Mutex::new(Some(question))))
    }

    pub fn take(&self) -> Option<ShareQuestion> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl PartialEq for QuestionSlot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// The app as the user knows it.
pub fn app_label(app_id: &str) -> String {
    if app_id.is_empty() {
        return "An application".into();
    }
    crate::windows::app_names().get(&app_id.to_lowercase()).cloned().unwrap_or_else(|| app_id.to_owned())
}

pub struct ScreenCastChooser {
    pub ui: mpsc::UnboundedSender<UiEvent>,
    pub busy: Arc<Mutex<bool>>,
}

impl ScreenCastChooser {
    /// Only the portal may ask, one question at a time (marks busy).
    async fn check(&self, header: &Header<'_>, conn: &Connection) -> fdo::Result<()> {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        let allowed = match (DBusProxy::new(conn).await, zbus::names::BusName::try_from(ALLOWED_CALLER)) {
            (Ok(dbus), Ok(name)) => dbus.get_name_owner(name).await.is_ok_and(|o| o.as_str() == sender),
            _ => false,
        };
        if !allowed {
            return Err(fdo::Error::AccessDenied("only the portal may ask".into()));
        }
        let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        if *busy {
            return Err(fdo::Error::Failed("another question is open".into()));
        }
        *busy = true;
        Ok(())
    }
}

#[interface(name = "org.helix.ScreenCastChooser")]
impl ScreenCastChooser {
    async fn allow_remote_control(
        &self,
        app_id: &str,
        devices: Vec<String>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<bool> {
        self.check(&header, conn).await?;
        let (reply, answer) = oneshot::channel();
        let question = ControlQuestion { app: app_label(app_id), devices, reply };
        let sent = self.ui.send(UiEvent::AllowControl(ControlSlot::new(question))).is_ok();
        let allowed = sent && answer.await.unwrap_or(false);
        *self.busy.lock().unwrap_or_else(|e| e.into_inner()) = false;
        tracing::info!(app_id, allowed, "remote control answered");
        Ok(allowed)
    }

    async fn choose_source(
        &self,
        app_id: &str,
        monitors: Vec<(String, String)>,
        options: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<(bool, u32, String, u32)> {
        let flag = |key: &str, default: bool| options.get(key).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(default);
        let monitors = if flag("monitors", true) { monitors } else { Vec::new() };
        let (windows, remember) = (flag("windows", false), flag("remember", false));
        let control: Vec<String> = options.get("control").and_then(|v| Vec::<String>::try_from(v.clone()).ok()).unwrap_or_default();
        self.check(&header, conn).await?;
        let (reply, answer) = oneshot::channel();
        let question = ShareQuestion { app: app_label(app_id), monitors, windows, remember, control, reply };
        let sent = self.ui.send(UiEvent::ShareScreen(QuestionSlot::new(question))).is_ok();
        let chosen = if sent { answer.await.ok().flatten() } else { None };
        *self.busy.lock().unwrap_or_else(|e| e.into_inner()) = false;
        tracing::info!(app_id, ?chosen, "screen share answered");
        Ok(match chosen {
            Some(Shared::Monitor(connector)) => (true, 1, connector, 0),
            Some(Shared::Window(id)) => (true, 2, String::new(), id),
            None => (false, 0, String::new(), 0),
        })
    }
}

/// Tell the UI whenever the compositor starts or stops sharing.
pub async fn follow_sharing(conn: Connection, ui: mpsc::UnboundedSender<UiEvent>) {
    let proxy = match zbus::Proxy::new(&conn, "org.gnome.Mutter.ScreenCast", "/org/gnome/Mutter/ScreenCast", "org.helix.ScreenCastState").await {
        Ok(p) => p,
        Err(_) => return,
    };
    let mut changes = proxy.receive_property_changed::<bool>("Active").await;
    if let Ok(active) = proxy.get_property::<bool>("Active").await {
        let _ = ui.send(UiEvent::Sharing(active));
    }
    while let Some(change) = changes.next().await {
        if let Ok(active) = change.get().await {
            let _ = ui.send(UiEvent::Sharing(active));
        }
    }
}
