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

/// A question for the UI: which of `monitors` (connector, label) to share
/// with `app`. The answer is the connector, or `None`.
#[derive(Debug)]
pub struct ShareQuestion {
    pub app: String,
    pub monitors: Vec<(String, String)>,
    pub reply: oneshot::Sender<Option<String>>,
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

#[interface(name = "org.helix.ScreenCastChooser")]
impl ScreenCastChooser {
    async fn choose_monitor(
        &self,
        app_id: &str,
        monitors: Vec<(String, String)>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<(bool, String)> {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        let allowed = match (DBusProxy::new(conn).await, zbus::names::BusName::try_from(ALLOWED_CALLER)) {
            (Ok(dbus), Ok(name)) => dbus.get_name_owner(name).await.is_ok_and(|o| o.as_str() == sender),
            _ => false,
        };
        if !allowed {
            return Err(fdo::Error::AccessDenied("only the portal may ask".into()));
        }
        {
            let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
            if *busy {
                return Err(fdo::Error::Failed("another question is open".into()));
            }
            *busy = true;
        }
        let (reply, answer) = oneshot::channel();
        let question = ShareQuestion { app: app_label(app_id), monitors, reply };
        let sent = self.ui.send(UiEvent::ShareScreen(QuestionSlot::new(question))).is_ok();
        let chosen = if sent { answer.await.ok().flatten() } else { None };
        *self.busy.lock().unwrap_or_else(|e| e.into_inner()) = false;
        tracing::info!(app_id, ?chosen, "screen share answered");
        Ok(match chosen {
            Some(connector) => (true, connector),
            None => (false, String::new()),
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
