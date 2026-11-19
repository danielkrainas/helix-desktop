//! `org.helixdesktop.DisplayChangeDialog`: the compositor asks whether to keep a
//! display configuration change (helixdesktop/spec
//! `org.helixdesktop.DisplayChangeDialog.md`).

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use zbus::message::Header;
use zbus::{interface, Connection};
use zbus::object_server::SignalEmitter;

use crate::UiEvent;

pub const OBJECT_PATH: &str = "/org/helixdesktop/DisplayChangeDialog";

pub struct DisplayChangeDialog {
    pub ui: mpsc::UnboundedSender<UiEvent>,
    /// Whether the question is showing (so an answer is expected).
    pub open: Arc<Mutex<bool>>,
}

impl DisplayChangeDialog {
    fn set_open(&self, open: bool) -> bool {
        std::mem::replace(&mut *self.open.lock().unwrap_or_else(|e| e.into_inner()), open)
    }

    /// The user answered (or the countdown ran out).
    pub async fn answer(&self, keep: bool, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        if !self.set_open(false) {
            return Ok(());
        }
        tracing::info!(keep, "display change answered");
        if keep {
            Self::kept(emitter).await
        } else {
            Self::reverted(emitter).await
        }
    }
}

/// Only the compositor (the owner of its `DisplayConfig` name) asks.
pub const ASKER: &str = "org.gnome.Mutter.DisplayConfig";

#[interface(name = "org.helixdesktop.DisplayChangeDialog")]
impl DisplayChangeDialog {
    async fn open(&self, seconds: u32, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> zbus::fdo::Result<()> {
        crate::caller::require_owner(conn, &header, &[ASKER]).await?;
        self.set_open(true);
        let _ = self.ui.send(UiEvent::DisplayChange(Some(seconds.max(1))));
        Ok(())
    }

    async fn close(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> zbus::fdo::Result<()> {
        crate::caller::require_owner(conn, &header, &[ASKER]).await?;
        if self.set_open(false) {
            let _ = self.ui.send(UiEvent::DisplayChange(None));
        }
        Ok(())
    }

    #[zbus(signal)]
    pub async fn kept(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn reverted(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}
