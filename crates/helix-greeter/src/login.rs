//! The login flow, driven by greetd's replies (helix-desktop-spec
//! `components/greeter.md`, "What the screen shows" and "Starting the
//! session"). Pure: it says which request to send next; the caller sends
//! it and feeds the reply back.
//!
//! greetd answers every request before it takes the next, so at most one
//! request is outstanding; [`Login::busy`] says when one is.

use crate::greetd::{AuthMessageType, ErrorType, Request, Response};

pub const RETRY_MESSAGE: &str = "Sorry, that didn't work. Please try again.";

/// A message shown under the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub text: String,
    pub error: bool,
}

/// A question greetd passed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub prompt: String,
    /// Hide what is typed.
    pub secret: bool,
}

/// What the outstanding request was.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Awaiting {
    Create,
    Answer,
    /// `restart`: begin again for this user once greetd has cancelled.
    Cancel { restart: Option<String> },
    Start,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// No login: the user list (or username field) is shown.
    Idle,
    /// A request is outstanding.
    Busy(Awaiting),
    /// A question waits for the user.
    Asking(Question),
    /// Authentication succeeded; the caller should send `start_session`
    /// through [`Login::start`].
    Authenticated,
    /// greetd accepted the session: the greeter exits.
    Started,
}

/// What the caller does after a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Send this request.
    Send(Request),
    /// Nothing to send; show the new state.
    Wait,
    /// Authentication succeeded: send `start_session` (see [`Login::start`]).
    Authenticated,
    /// The session will start: save state and exit.
    Started,
}

#[derive(Debug, Clone)]
pub struct Login {
    user: Option<String>,
    phase: Phase,
    notes: Vec<Note>,
    /// Escape while a request was outstanding: cancel when it is answered.
    cancel_pending: bool,
}

impl Default for Login {
    fn default() -> Self {
        Self { user: None, phase: Phase::Idle, notes: Vec::new(), cancel_pending: false }
    }
}

impl Login {
    /// The user being logged in, if any.
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// Whether the user list (or username field) is shown: no login.
    pub fn idle(&self) -> bool {
        self.phase == Phase::Idle
    }

    /// Waiting for greetd.
    pub fn busy(&self) -> bool {
        matches!(self.phase, Phase::Busy(_))
    }

    pub fn question(&self) -> Option<&Question> {
        match &self.phase {
            Phase::Asking(q) => Some(q),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn authenticated(&self) -> bool {
        self.phase == Phase::Authenticated
    }

    #[cfg(test)]
    pub fn started(&self) -> bool {
        self.phase == Phase::Started
    }

    pub fn notes(&self) -> &[Note] {
        &self.notes
    }

    /// Begin a login for `user` (from the list or the username field).
    pub fn begin(&mut self, user: &str) -> Option<Request> {
        if self.phase != Phase::Idle {
            return None;
        }
        self.notes.clear();
        self.user = Some(user.to_owned());
        self.cancel_pending = false;
        self.phase = Phase::Busy(Awaiting::Create);
        Some(Request::CreateSession { username: user.to_owned() })
    }

    /// Answer the question shown.
    pub fn answer(&mut self, text: &str) -> Option<Request> {
        self.question()?;
        self.notes.clear();
        self.phase = Phase::Busy(Awaiting::Answer);
        Some(Request::PostAuthMessageResponse { response: Some(text.to_owned()) })
    }

    /// Escape or Back: give up the login and return to the user list.
    pub fn cancel(&mut self) -> Option<Request> {
        match &self.phase {
            Phase::Idle | Phase::Started | Phase::Busy(Awaiting::Start) => None,
            Phase::Busy(Awaiting::Cancel { restart }) => {
                // Already cancelling: just don't begin again.
                if restart.is_some() {
                    self.phase = Phase::Busy(Awaiting::Cancel { restart: None });
                }
                None
            }
            Phase::Busy(_) => {
                self.cancel_pending = true;
                None
            }
            Phase::Asking(_) | Phase::Authenticated => {
                self.notes.clear();
                self.phase = Phase::Busy(Awaiting::Cancel { restart: None });
                Some(Request::CancelSession)
            }
        }
    }

    /// Authentication succeeded: send the session to start (built by
    /// [`crate::sessions::start_request`]).
    pub fn start(&mut self, request: Request) -> Option<Request> {
        if self.phase != Phase::Authenticated {
            return None;
        }
        self.phase = Phase::Busy(Awaiting::Start);
        Some(request)
    }

    /// Authentication succeeded but no session can be started: show
    /// `message` and give up the login.
    pub fn abandon(&mut self, message: &str) -> Option<Request> {
        if self.phase != Phase::Authenticated {
            return None;
        }
        self.notes = vec![Note { text: message.to_owned(), error: true }];
        self.phase = Phase::Busy(Awaiting::Cancel { restart: None });
        Some(Request::CancelSession)
    }

    fn cancel_now(&mut self, restart: Option<String>) -> Step {
        self.phase = Phase::Busy(Awaiting::Cancel { restart });
        Step::Send(Request::CancelSession)
    }

    /// greetd's reply to the outstanding request.
    pub fn reply(&mut self, response: Response) -> Step {
        let Phase::Busy(awaiting) = self.phase.clone() else {
            tracing::warn!(?response, "greetd reply with no request outstanding");
            return Step::Wait;
        };
        match awaiting {
            Awaiting::Cancel { restart } => match restart {
                Some(user) => {
                    self.phase = Phase::Busy(Awaiting::Create);
                    self.user = Some(user.clone());
                    Step::Send(Request::CreateSession { username: user })
                }
                None => {
                    self.phase = Phase::Idle;
                    self.user = None;
                    Step::Wait
                }
            },
            Awaiting::Start => match response {
                Response::Success => {
                    self.phase = Phase::Started;
                    Step::Started
                }
                Response::Error { description, .. } => {
                    self.notes = vec![Note { text: description, error: true }];
                    self.cancel_now(None)
                }
                Response::AuthMessage { .. } => {
                    tracing::warn!("greetd asked a question after start_session");
                    self.cancel_now(None)
                }
            },
            Awaiting::Create | Awaiting::Answer => {
                if std::mem::take(&mut self.cancel_pending) {
                    self.notes.clear();
                    return self.cancel_now(None);
                }
                match response {
                    Response::Success => {
                        self.phase = Phase::Authenticated;
                        Step::Authenticated
                    }
                    Response::AuthMessage { auth_message_type, auth_message } => match auth_message_type {
                        AuthMessageType::Visible | AuthMessageType::Secret => {
                            let prompt = auth_message.trim().to_owned();
                            self.phase = Phase::Asking(Question { prompt, secret: auth_message_type == AuthMessageType::Secret });
                            Step::Wait
                        }
                        AuthMessageType::Info | AuthMessageType::Error => {
                            // Shown, and answered at once.
                            self.notes.push(Note { text: auth_message.trim().to_owned(), error: auth_message_type == AuthMessageType::Error });
                            self.phase = Phase::Busy(Awaiting::Answer);
                            Step::Send(Request::PostAuthMessageResponse { response: None })
                        }
                    },
                    Response::Error { error_type: ErrorType::AuthError, .. } => {
                        // Try again for the same user.
                        self.notes = vec![Note { text: RETRY_MESSAGE.into(), error: true }];
                        let user = self.user.clone();
                        self.cancel_now(user)
                    }
                    Response::Error { description, .. } => {
                        self.notes = vec![Note { text: description, error: true }];
                        self.cancel_now(None)
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(prompt: &str) -> Response {
        Response::AuthMessage { auth_message_type: AuthMessageType::Secret, auth_message: prompt.into() }
    }

    fn start_request() -> Request {
        Request::StartSession { cmd: vec!["/bin/sh".into(), "-c".into(), "exec helix-session".into()], env: vec![] }
    }

    #[test]
    fn password_login() {
        let mut login = Login::default();
        assert!(login.idle());
        assert_eq!(login.begin("ann"), Some(Request::CreateSession { username: "ann".into() }));
        assert!(login.busy());
        assert_eq!(login.begin("bob"), None, "one login at a time");
        assert_eq!(login.reply(secret("Password: ")), Step::Wait);
        assert_eq!(login.question(), Some(&Question { prompt: "Password:".into(), secret: true }));
        assert_eq!(login.answer("pw"), Some(Request::PostAuthMessageResponse { response: Some("pw".into()) }));
        assert!(login.busy());
        assert_eq!(login.reply(Response::Success), Step::Authenticated);
        assert!(login.authenticated());
        assert_eq!(login.start(start_request()), Some(start_request()));
        assert_eq!(login.reply(Response::Success), Step::Started);
        assert!(login.started());
        assert_eq!(login.user(), Some("ann"));
    }

    #[test]
    fn wrong_password_retries_same_user() {
        let mut login = Login::default();
        login.begin("ann");
        login.reply(secret("Password:"));
        login.answer("wrong");
        let step = login.reply(Response::Error { error_type: ErrorType::AuthError, description: "AUTH_ERR".into() });
        assert_eq!(step, Step::Send(Request::CancelSession));
        assert_eq!(login.notes(), &[Note { text: RETRY_MESSAGE.into(), error: true }]);
        // After greetd cancels, the login begins again for the same user.
        assert_eq!(login.reply(Response::Success), Step::Send(Request::CreateSession { username: "ann".into() }));
        assert_eq!(login.reply(secret("Password:")), Step::Wait);
        assert!(login.question().is_some());
        assert_eq!(login.notes().len(), 1, "the retry message stays with the new question");
        login.answer("right");
        assert!(login.notes().is_empty(), "cleared once answered");
    }

    #[test]
    fn other_errors_return_to_the_list() {
        let mut login = Login::default();
        login.begin("ann");
        assert_eq!(login.reply(Response::Error { error_type: ErrorType::Error, description: "Account expired".into() }), Step::Send(Request::CancelSession));
        assert_eq!(login.reply(Response::Success), Step::Wait);
        assert!(login.idle());
        assert_eq!(login.notes()[0].text, "Account expired");
        assert_eq!(login.user(), None);
    }

    #[test]
    fn info_and_error_messages_are_answered_at_once() {
        let mut login = Login::default();
        login.begin("ann");
        let info = Response::AuthMessage { auth_message_type: AuthMessageType::Info, auth_message: "Password expires in 3 days".into() };
        assert_eq!(login.reply(info), Step::Send(Request::PostAuthMessageResponse { response: None }));
        let error = Response::AuthMessage { auth_message_type: AuthMessageType::Error, auth_message: "Fingerprint not recognised".into() };
        assert_eq!(login.reply(error), Step::Send(Request::PostAuthMessageResponse { response: None }));
        let visible = Response::AuthMessage { auth_message_type: AuthMessageType::Visible, auth_message: "Token:".into() };
        assert_eq!(login.reply(visible), Step::Wait);
        assert_eq!(login.question(), Some(&Question { prompt: "Token:".into(), secret: false }));
        assert_eq!(login.notes(), &[Note { text: "Password expires in 3 days".into(), error: false }, Note { text: "Fingerprint not recognised".into(), error: true }]);
    }

    #[test]
    fn escape_cancels() {
        let mut login = Login::default();
        assert_eq!(login.cancel(), None, "nothing to cancel");
        login.begin("ann");
        login.reply(secret("Password:"));
        assert_eq!(login.cancel(), Some(Request::CancelSession));
        assert_eq!(login.reply(Response::Success), Step::Wait);
        assert!(login.idle());
        // Escape while greetd is busy: cancelled when it answers.
        login.begin("ann");
        assert_eq!(login.cancel(), None);
        assert_eq!(login.reply(secret("Password:")), Step::Send(Request::CancelSession));
        assert_eq!(login.reply(Response::Success), Step::Wait);
        assert!(login.idle());
        // Escape while cancelling after a wrong password: don't begin again.
        login.begin("ann");
        login.reply(secret("Password:"));
        login.answer("x");
        login.reply(Response::Error { error_type: ErrorType::AuthError, description: String::new() });
        assert_eq!(login.cancel(), None);
        assert_eq!(login.reply(Response::Success), Step::Wait);
        assert!(login.idle());
    }

    #[test]
    fn start_failure_returns_to_the_list() {
        let mut login = Login::default();
        login.begin("ann");
        login.reply(Response::Success);
        assert_eq!(login.start(start_request()), Some(start_request()));
        assert_eq!(login.reply(Response::Error { error_type: ErrorType::Error, description: "bad cmd".into() }), Step::Send(Request::CancelSession));
        login.reply(Response::Success);
        assert!(login.idle());
        assert_eq!(login.notes()[0].text, "bad cmd");
    }
}
