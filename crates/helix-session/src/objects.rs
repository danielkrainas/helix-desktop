//! Per-client and per-inhibitor D-Bus objects.
//!
//! Contracts: `org.gnome.SessionManager.ClientPrivate.xml` and
//! `org.gnome.SessionManager.Inhibitor.xml` in helix-desktop-spec.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedObjectPath;

/// Clients' answers to QueryEndSession / EndSession, by client path.
#[derive(Clone, Default)]
pub struct Responses {
    answers: Arc<Mutex<HashMap<String, (bool, String)>>>,
    pub changed: Arc<Notify>,
}

impl Responses {
    pub fn clear(&self) {
        self.answers.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    pub fn get(&self, client: &str) -> Option<(bool, String)> {
        self.answers.lock().unwrap_or_else(|e| e.into_inner()).get(client).cloned()
    }

    fn record(&self, client: &str, is_ok: bool, reason: &str) {
        self.answers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(client.to_owned(), (is_ok, reason.to_owned()));
        self.changed.notify_waiters();
    }
}

/// `org.gnome.SessionManager.ClientPrivate` at `/org/gnome/SessionManager/Client<n>`.
pub struct ClientObject {
    pub path: String,
    pub responses: Responses,
}

#[interface(name = "org.gnome.SessionManager.ClientPrivate")]
impl ClientObject {
    async fn end_session_response(&self, is_ok: bool, reason: &str) {
        tracing::debug!(client = %self.path, is_ok, reason, "end session response");
        self.responses.record(&self.path, is_ok, reason);
    }

    #[zbus(signal)]
    pub async fn stop(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn query_end_session(emitter: &SignalEmitter<'_>, flags: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn end_session(emitter: &SignalEmitter<'_>, flags: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn cancel_end_session(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// `org.gnome.SessionManager.Inhibitor` at `/org/gnome/SessionManager/Inhibitor<cookie>`.
pub struct InhibitorObject {
    pub app_id: String,
    pub reason: String,
    pub flags: u32,
    /// The inhibiting app's client object, or `/`.
    pub client: OwnedObjectPath,
}

#[interface(name = "org.gnome.SessionManager.Inhibitor")]
impl InhibitorObject {
    async fn get_app_id(&self) -> String {
        self.app_id.clone()
    }

    async fn get_client_id(&self) -> OwnedObjectPath {
        self.client.clone()
    }

    async fn get_reason(&self) -> String {
        self.reason.clone()
    }

    async fn get_flags(&self) -> u32 {
        self.flags
    }
}
