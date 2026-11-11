//! The end-of-session sequence (helix-desktop-spec
//! `org.gnome.SessionManager.md`, "Ending the session"):
//! confirm with the shell's dialog, query clients, end, stop, then
//! `SessionOver`.

use std::time::Duration;

use futures_util::StreamExt;
use tokio::time::Instant;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::{Connection, MessageStream};

use crate::manager::{SessionManager, OBJECT_PATH};
use crate::objects::{ClientObject, Responses};
use crate::state::{inhibit_flags, EndSessionAction, LogoutMode};

pub const DIALOG_BUS_NAME: &str = "org.gnome.Shell";
pub const DIALOG_PATH: &str = "/org/gnome/SessionManager/EndSessionDialog";
const DIALOG_IFACE: &str = "org.gnome.SessionManager.EndSessionDialog";
/// GNOME's confirmation countdown.
const DIALOG_SECONDS: u32 = 60;
/// QueryEndSession / EndSession flag: the end is forced.
const FLAG_FORCED: u32 = 1;

/// How long each step may take.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub query: Duration,
    pub end: Duration,
    /// Give up waiting for the dialog (e.g. the shell died) after this.
    pub confirm: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            query: Duration::from_secs(1),
            end: Duration::from_secs(10),
            confirm: Duration::from_secs(5 * 60),
        }
    }
}

/// EndSessionDialog `Open` type for an action.
pub fn dialog_type(action: EndSessionAction) -> u32 {
    match action {
        EndSessionAction::Shutdown => 1,
        EndSessionAction::Reboot => 2,
        _ => 0,
    }
}

/// Whether the dialog must be asked before ending.
pub fn needs_confirmation(action: EndSessionAction, blocked: bool) -> bool {
    match action {
        EndSessionAction::Logout(LogoutMode::Force) | EndSessionAction::InitializationFailed => false,
        EndSessionAction::Logout(LogoutMode::NoConfirmation) => blocked,
        _ => true,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Confirmed,
    Canceled,
    /// A forced logout arrived meanwhile.
    Forced,
}

/// Lets a forced logout take over a sequence waiting for the dialog.
#[derive(Clone, Default)]
pub struct ForceSignal {
    forced: std::sync::Arc<std::sync::atomic::AtomicBool>,
    notify: std::sync::Arc<tokio::sync::Notify>,
}

impl ForceSignal {
    pub fn force(&self) {
        self.forced.store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_forced(&self) -> bool {
        self.forced.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn reset(&self) {
        self.forced.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_forced() {
                return;
            }
            notified.await;
        }
    }
}

/// The dialog, while we wait for it.
pub struct Dialog {
    signals: MessageStream,
}

impl Dialog {
    /// Subscribe, then open (or update) the dialog. Errors mean there is no
    /// dialog to ask.
    pub async fn open(conn: &Connection, action: EndSessionAction, blockers: &[OwnedObjectPath]) -> zbus::Result<Self> {
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(DIALOG_IFACE)?
            .path(DIALOG_PATH)?
            .build();
        let signals = MessageStream::for_match_rule(rule, conn, None).await?;
        let paths: Vec<ObjectPath<'_>> = blockers.iter().map(|p| p.as_ref()).collect();
        conn.call_method(
            Some(DIALOG_BUS_NAME),
            DIALOG_PATH,
            Some(DIALOG_IFACE),
            "Open",
            &(dialog_type(action), 0u32, DIALOG_SECONDS, paths),
        )
        .await?;
        Ok(Self { signals })
    }

    pub async fn decision(self, timeout: Duration, force: &ForceSignal) -> Decision {
        tokio::select! {
            d = self.answer(timeout) => d,
            () = force.wait() => Decision::Forced,
        }
    }

    async fn answer(mut self, timeout: Duration) -> Decision {
        let wait = async {
            while let Some(Ok(message)) = self.signals.next().await {
                let header = message.header();
                match header.member().map(|m| m.as_str()) {
                    Some("ConfirmedLogout" | "ConfirmedShutdown" | "ConfirmedReboot") => return Decision::Confirmed,
                    Some("Canceled" | "Closed") => return Decision::Canceled,
                    _ => {}
                }
            }
            Decision::Canceled
        };
        tokio::time::timeout(timeout, wait).await.unwrap_or(Decision::Canceled)
    }
}

/// Emit a ClientPrivate signal on every client object.
async fn to_clients<F, Fut>(conn: &Connection, clients: &[String], emit: F)
where
    F: Fn(SignalEmitter<'static>) -> Fut,
    Fut: std::future::Future<Output = zbus::Result<()>>,
{
    for path in clients {
        match SignalEmitter::new(conn, path.clone()) {
            Ok(emitter) => {
                if let Err(err) = emit(emitter.into_owned()).await {
                    tracing::warn!(%err, client = %path, "cannot signal client");
                }
            }
            Err(err) => tracing::warn!(%err, "bad client path"),
        }
    }
}

/// Wait until every client answered or the timeout passed.
async fn wait_for_answers(responses: &Responses, clients: &[String], timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let notified = responses.changed.notified();
        if clients.iter().all(|c| responses.get(c).is_some()) {
            return;
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return;
        }
    }
}

/// Everything the sequence needs, detached from the D-Bus method call.
pub struct Sequence {
    pub conn: Connection,
    pub action: EndSessionAction,
    pub responses: Responses,
    pub timeouts: Timeouts,
    /// The dialog was opened by the caller (confirmation pending).
    pub dialog: Option<Dialog>,
    pub force: ForceSignal,
}

impl Sequence {
    pub async fn run(self) {
        if let Err(err) = self.steps().await {
            tracing::warn!(%err, "end-of-session sequence failed; the session keeps running");
            self.with_manager(|m| m.cancel_end()).await;
        }
    }

    async fn with_manager<T>(&self, f: impl FnOnce(&SessionManager) -> T) -> Option<T> {
        let iface = self.conn.object_server().interface::<_, SessionManager>(OBJECT_PATH).await.ok()?;
        let manager = iface.get().await;
        Some(f(&manager))
    }

    async fn steps(&self) -> zbus::Result<()> {
        let forced = self.action == EndSessionAction::Logout(LogoutMode::Force) || self.force.is_forced();
        let clients = self.with_manager(|m| m.client_paths()).await.unwrap_or_default();

        if !forced {
            // Query: anyone not ready?
            self.responses.clear();
            to_clients(&self.conn, &clients, |e| async move { ClientObject::query_end_session(&e, 0).await }).await;
            wait_for_answers(&self.responses, &clients, self.timeouts.query).await;
            let refusals: Vec<(String, String)> = clients
                .iter()
                .filter_map(|c| match self.responses.get(c) {
                    Some((false, reason)) => Some((c.clone(), reason)),
                    _ => None,
                })
                .collect();
            if !refusals.is_empty() {
                tracing::info!(count = refusals.len(), "clients asked to keep the session");
                // Show refusals like inhibitors: temporary Inhibitor objects.
                let iface = self.conn.object_server().interface::<_, SessionManager>(OBJECT_PATH).await?;
                let mut temporary = Vec::new();
                for (client, reason) in &refusals {
                    if let Some(path) = iface.get().await.add_refusal(&self.conn, client, reason).await {
                        temporary.push(path);
                    }
                }
                let mut blockers = self.with_manager(|m| m.blocking_paths()).await.unwrap_or_default();
                blockers.retain(|p| !temporary.contains(p));
                blockers.extend(temporary.iter().cloned());
                let decision = match self.dialog_for(&blockers).await {
                    Some(dialog) => dialog.decision(self.timeouts.confirm, &self.force).await,
                    None => Decision::Canceled,
                };
                if decision == Decision::Forced {
                    close_dialog(&self.conn).await;
                }
                for path in &temporary {
                    iface.get().await.remove_refusal(&self.conn, path).await;
                }
                if decision == Decision::Canceled {
                    tracing::info!("ending the session was cancelled");
                    to_clients(&self.conn, &clients, |e| async move { ClientObject::cancel_end_session(&e).await }).await;
                    self.with_manager(|m| m.cancel_end()).await;
                    return Ok(());
                }
            }
        }

        // End: clients get time to clean up.
        let flags = if forced { FLAG_FORCED } else { 0 };
        self.responses.clear();
        to_clients(&self.conn, &clients, |e| async move { ClientObject::end_session(&e, flags).await }).await;
        wait_for_answers(&self.responses, &clients, self.timeouts.end).await;

        // Stop, then the session is over.
        to_clients(&self.conn, &clients, |e| async move { ClientObject::stop(&e).await }).await;
        let iface = self.conn.object_server().interface::<_, SessionManager>(OBJECT_PATH).await?;
        let manager = iface.get().await;
        manager.finish_end(self.action, iface.signal_emitter()).await
    }

    async fn dialog_for(&self, blockers: &[OwnedObjectPath]) -> Option<Dialog> {
        match Dialog::open(&self.conn, self.action, blockers).await {
            Ok(dialog) => Some(dialog),
            Err(err) => {
                tracing::warn!(%err, "no end-session dialog to show refusals; cancelling");
                None
            }
        }
    }
}

/// Dismiss the dialog (a forced logout took over).
async fn close_dialog(conn: &Connection) {
    let _ = conn
        .call_method(Some(DIALOG_BUS_NAME), DIALOG_PATH, Some(DIALOG_IFACE), "Close", &())
        .await;
}

/// Run the sequence, first waiting for an already-opened dialog.
pub async fn run(mut sequence: Sequence) {
    if let Some(dialog) = sequence.dialog.take() {
        match dialog.decision(sequence.timeouts.confirm, &sequence.force).await {
            Decision::Canceled => {
                tracing::info!("ending the session was cancelled");
                sequence.with_manager(|m| m.cancel_end()).await;
                return;
            }
            Decision::Forced => {
                tracing::info!("forced logout took over the pending confirmation");
                close_dialog(&sequence.conn).await;
            }
            Decision::Confirmed => {}
        }
    }
    sequence.run().await;
}

/// Inhibitor flags for a client refusal shown in the dialog.
pub const REFUSAL_FLAGS: u32 = inhibit_flags::LOGOUT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_types_and_confirmation_rules() {
        assert_eq!(dialog_type(EndSessionAction::Logout(LogoutMode::Normal)), 0);
        assert_eq!(dialog_type(EndSessionAction::Shutdown), 1);
        assert_eq!(dialog_type(EndSessionAction::Reboot), 2);

        assert!(needs_confirmation(EndSessionAction::Logout(LogoutMode::Normal), false));
        assert!(needs_confirmation(EndSessionAction::Shutdown, false));
        assert!(!needs_confirmation(EndSessionAction::Logout(LogoutMode::NoConfirmation), false));
        assert!(needs_confirmation(EndSessionAction::Logout(LogoutMode::NoConfirmation), true));
        assert!(!needs_confirmation(EndSessionAction::Logout(LogoutMode::Force), true));
    }
}
