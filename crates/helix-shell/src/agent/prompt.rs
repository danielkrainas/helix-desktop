//! The consent prompt, the sensitive-action prompt and the indicator
//! (helixdesktop/spec `contracts/agent-access/README.md`, "Consent",
//! "Sensitive actions" and "Indicator"; helixdesktop/desktop#143): the
//! hooks [`AgentAccess`](super::AgentAccess) calls, carried to the UI
//! thread as [`UiEvent`]s.
//!
//! The answer travels back only on the oneshot channel the UI holds: no
//! D-Bus method answers a prompt, and the UI ignores accessibility
//! actions and remote input on it (see `ui/agent_prompt.rs`).

use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use super::grants::Answer;
use super::{Confirm, ConfirmRequest, Consent, ConsentRequest, Level};
use crate::UiEvent;

/// What a prompt asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Question {
    /// May the agent have `level`? *Always allow* offered or not.
    Consent { level: Level, offer_always: bool },
    /// May the agent do this sensitive action (an `Agent1` method)?
    Confirm { method: &'static str },
}

/// A prompt for the UI.
#[derive(Debug)]
pub struct Request {
    /// The agent's self-declared display name ("" for none).
    pub name: String,
    /// Its id (the executable, or the interpreter and the script).
    pub agent: String,
    pub question: Question,
    pub reply: oneshot::Sender<Answer>,
}

/// A [`Request`] that can travel in a [`UiEvent`].
#[derive(Debug, Clone)]
pub struct RequestSlot(Arc<Mutex<Option<Request>>>);

impl RequestSlot {
    pub fn new(request: Request) -> Self {
        Self(Arc::new(Mutex::new(Some(request))))
    }

    pub fn take(&self) -> Option<Request> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl PartialEq for RequestSlot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Show `request`'s prompt and wait for the answer; [`Answer::Unanswered`]
/// when the UI is gone or drops the prompt (the screen locked, the shell
/// ended).
async fn ask(ui: &mpsc::UnboundedSender<UiEvent>, name: String, agent: String, question: Question) -> Answer {
    let (reply, answer) = oneshot::channel();
    let slot = RequestSlot::new(Request { name, agent, question, reply });
    if ui.send(UiEvent::AgentPrompt(slot)).is_err() {
        return Answer::Unanswered;
    }
    answer.await.unwrap_or(Answer::Unanswered)
}

/// The consent hook: the shell's consent prompt.
pub fn consent(ui: mpsc::UnboundedSender<UiEvent>) -> Consent {
    Arc::new(move |request: ConsentRequest| {
        let ui = ui.clone();
        Box::pin(async move {
            let question = Question::Consent { level: request.level, offer_always: request.offer_always };
            ask(&ui, request.agent.name, request.agent.id, question).await
        })
    })
}

/// The sensitive-action hook: `Some(true)` allowed, `Some(false)` denied,
/// `None` unanswered.
pub fn confirm(ui: mpsc::UnboundedSender<UiEvent>) -> Confirm {
    Arc::new(move |request: ConfirmRequest| {
        let ui = ui.clone();
        Box::pin(async move {
            match ask(&ui, request.agent.name, request.agent.id, Question::Confirm { method: request.method }).await {
                Answer::Session | Answer::Always => Some(true),
                Answer::Deny => Some(false),
                Answer::Unanswered => None,
            }
        })
    })
}

/// An agent the indicator lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Active {
    pub agent: String,
    pub executable: String,
    pub name: String,
    /// The level it may use now.
    pub level: Level,
}

/// How long after its last allowed call an agent counts as active.
pub const ACTIVE_FOR: i64 = 60;

/// A path's last part.
fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

impl Active {
    /// How menus name it, short: the display name and the executable's
    /// file name, or the id's file names ("python3 bot.py").
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            self.agent.split(' ').map(base).collect::<Vec<_>>().join(" ")
        } else {
            format!("{} ({})", self.name, base(&self.executable))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        let a = |agent: &str, name: &str| Active { agent: agent.into(), executable: agent.split(' ').next().unwrap().into(), name: name.into(), level: Level::ReadOnly };
        assert_eq!(a("/usr/bin/python3 /home/me/bot.py", "").label(), "python3 bot.py");
        assert_eq!(a("/usr/bin/python3 /home/me/bot.py", "Build bot").label(), "Build bot (python3)");
        assert_eq!(a("/opt/agent/agent", "").label(), "agent");
    }
}
