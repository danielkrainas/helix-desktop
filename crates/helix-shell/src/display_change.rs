//! `org.helix.DisplayChangeDialog`: the compositor asks whether to keep a
//! display configuration change (helix-desktop-spec
//! `org.helix.DisplayChangeDialog.md`).

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use zbus::interface;
use zbus::object_server::SignalEmitter;

use crate::UiEvent;

pub const OBJECT_PATH: &str = "/org/helix/DisplayChangeDialog";

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

#[interface(name = "org.helix.DisplayChangeDialog")]
impl DisplayChangeDialog {
    async fn open(&self, seconds: u32) {
        self.set_open(true);
        let _ = self.ui.send(UiEvent::DisplayChange(Some(seconds.max(1))));
    }

    async fn close(&self) {
        if self.set_open(false) {
            let _ = self.ui.send(UiEvent::DisplayChange(None));
        }
    }

    #[zbus(signal)]
    pub async fn kept(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn reverted(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}
