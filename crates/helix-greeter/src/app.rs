//! The greeter's state and input handling, apart from Wayland: keys and
//! clicks in, greetd requests and power actions out, and the view the
//! painter draws (helix-desktop-spec `components/greeter.md`).

use crate::config::Config;
use crate::greetd::{Request, Response};
use crate::login::{Login, Note, Question, Step};
use crate::power::{Offered, PowerAction};
use crate::sessions::{self, Session};
use crate::state::State;
use crate::users::User;

/// A key, after the keymap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Return,
    Escape,
    Tab,
    BackTab,
    Backspace,
    /// Typed text.
    Text(String),
}

/// What has keyboard focus (Tab cycles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The user list or the field.
    Main,
    Session,
    /// The n-th power button shown.
    Power(usize),
}

/// What a click landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    User(usize),
    Field,
    Back,
    SessionButton,
    SessionItem(usize),
    Power(PowerAction),
    PowerCancel,
    PowerConfirm,
}

/// What the caller does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Greetd(Request),
    Power(PowerAction),
    /// greetd will start the session: remember this, then exit with 0.
    Finished(State),
}

/// The power buttons shown at the bottom left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerView {
    Buttons(Offered),
    /// "Restart this computer?" with Cancel and the action.
    Confirm(PowerAction),
}

impl PowerView {
    /// The buttons, in order (what `Focus::Power(n)` counts).
    pub fn hits(self) -> Vec<Hit> {
        match self {
            Self::Buttons(o) => [(o.restart, PowerAction::Restart), (o.power_off, PowerAction::PowerOff)].into_iter().filter(|(on, _)| *on).map(|(_, a)| Hit::Power(a)).collect(),
            Self::Confirm(_) => vec![Hit::PowerCancel, Hit::PowerConfirm],
        }
    }
}

/// The middle of the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Main {
    /// Users not known yet.
    Loading,
    Users { names: Vec<String>, selected: usize },
    Username { text: String },
    Login {
        user: String,
        /// The question; `None` before greetd asks one.
        question: Option<Question>,
        /// Typed so far (shown as dots for secret questions).
        text: String,
        /// Waiting for greetd: the field takes no typing.
        busy: bool,
    },
}

/// Everything painted, for [`crate::paint::paint`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub time: String,
    pub date: String,
    pub banner: Option<String>,
    pub main: Main,
    pub notes: Vec<Note>,
    pub sessions: Vec<String>,
    pub session: usize,
    /// The session menu, open, with its highlighted item.
    pub session_menu: Option<usize>,
    pub power: PowerView,
    pub focus: Focus,
    pub hover: Option<Hit>,
}

pub struct Greeter {
    config: Config,
    /// `None` until loaded.
    users: Option<Vec<User>>,
    selected: usize,
    /// Typed name when there is no list.
    username: String,
    sessions: Vec<Session>,
    session: usize,
    session_menu: Option<usize>,
    power: Offered,
    confirming: Option<PowerAction>,
    login: Login,
    /// The question shown, kept while its answer is checked.
    question: Option<Question>,
    answer: String,
    /// Display name of the user logging in.
    login_display: String,
    last_user: Option<String>,
    pub focus: Focus,
    pub hover: Option<Hit>,
}

impl Greeter {
    pub fn new(config: Config, sessions: Vec<Session>, state: &State) -> Self {
        let session = sessions::default_index(&sessions, state.last_session.as_deref());
        Self {
            config,
            users: None,
            selected: 0,
            username: String::new(),
            sessions,
            session,
            session_menu: None,
            power: Offered::default(),
            confirming: None,
            login: Login::default(),
            question: None,
            answer: String::new(),
            login_display: String::new(),
            last_user: state.last_user.clone(),
            focus: Focus::Main,
            hover: None,
        }
    }

    /// The users AccountsService or `/etc/passwd` gave; the last user to
    /// log in is selected.
    pub fn set_users(&mut self, users: Vec<User>) {
        self.selected = self.last_user.as_deref().and_then(|last| users.iter().position(|u| u.name == last)).unwrap_or(0);
        self.users = Some(users);
    }

    pub fn set_power(&mut self, offered: Offered) {
        self.power = offered;
        self.clamp_focus();
    }

    /// Whether the username field is shown instead of the list.
    fn ask_username(&self) -> bool {
        !self.config.user_list || self.users.as_ref().is_some_and(|u| u.is_empty())
    }

    fn list(&self) -> &[User] {
        if self.config.user_list { self.users.as_deref().unwrap_or_default() } else { &[] }
    }

    fn power_view(&self) -> PowerView {
        match self.confirming {
            Some(action) => PowerView::Confirm(action),
            None => PowerView::Buttons(self.power),
        }
    }

    /// The focus cycle: main, session button, power buttons.
    fn focus_order(&self) -> Vec<Focus> {
        let mut order = vec![Focus::Main];
        if !self.sessions.is_empty() {
            order.push(Focus::Session);
        }
        order.extend((0..self.power_view().hits().len()).map(Focus::Power));
        order
    }

    fn clamp_focus(&mut self) {
        if !self.focus_order().contains(&self.focus) {
            self.focus = Focus::Main;
        }
    }

    fn begin(&mut self, name: &str, display: &str) -> Vec<Effect> {
        let Some(request) = self.login.begin(name) else {
            return Vec::new();
        };
        self.login_display = display.to_owned();
        self.question = None;
        self.answer.clear();
        self.focus = Focus::Main;
        vec![Effect::Greetd(request)]
    }

    fn begin_selected(&mut self) -> Vec<Effect> {
        if self.ask_username() {
            let name = self.username.trim().to_owned();
            if name.is_empty() {
                return Vec::new();
            }
            return self.begin(&name, &name);
        }
        let Some(user) = self.list().get(self.selected).cloned() else {
            return Vec::new();
        };
        self.begin(&user.name, &user.display)
    }

    fn cancel_login(&mut self) -> Vec<Effect> {
        let request = self.login.cancel();
        request.map(Effect::Greetd).into_iter().collect()
    }

    /// Keep the shown question and typed text in step with the login
    /// after a reply: each question greetd asks gets a fresh field.
    fn after_login_change(&mut self) {
        if let Some(q) = self.login.question() {
            self.question = Some(q.clone());
            self.answer.clear();
        } else if self.login.idle() {
            self.question = None;
            self.answer.clear();
        }
    }

    fn activate(&mut self, hit: Hit) -> Vec<Effect> {
        match hit {
            Hit::User(i) => {
                self.focus = Focus::Main;
                if self.login.idle() && i < self.list().len() {
                    self.selected = i;
                    return self.begin_selected();
                }
            }
            Hit::Field => self.focus = Focus::Main,
            Hit::Back => return self.cancel_login(),
            Hit::SessionButton => {
                self.focus = Focus::Session;
                self.session_menu = if self.session_menu.is_some() || self.sessions.is_empty() { None } else { Some(self.session) };
            }
            Hit::SessionItem(i) => {
                if i < self.sessions.len() {
                    self.session = i;
                }
                self.session_menu = None;
                self.focus = Focus::Session;
            }
            Hit::Power(action) => {
                self.confirming = Some(action);
                // The action, so Return twice does it.
                self.focus = Focus::Power(1);
            }
            Hit::PowerCancel => {
                self.confirming = None;
                self.focus = Focus::Main;
            }
            Hit::PowerConfirm => {
                if let Some(action) = self.confirming.take() {
                    self.focus = Focus::Main;
                    return vec![Effect::Power(action)];
                }
            }
        }
        Vec::new()
    }

    pub fn click(&mut self, hit: Option<Hit>) -> Vec<Effect> {
        if self.session_menu.is_some() && !matches!(hit, Some(Hit::SessionItem(_) | Hit::SessionButton)) {
            self.session_menu = None;
            return Vec::new();
        }
        match hit {
            Some(hit) => self.activate(hit),
            None => Vec::new(),
        }
    }

    pub fn key(&mut self, key: Key) -> Vec<Effect> {
        if let Some(highlighted) = self.session_menu {
            let last = self.sessions.len().saturating_sub(1);
            match key {
                Key::Up => self.session_menu = Some(highlighted.saturating_sub(1)),
                Key::Down => self.session_menu = Some((highlighted + 1).min(last)),
                Key::Return => return self.activate(Hit::SessionItem(highlighted)),
                Key::Text(t) if t == " " => return self.activate(Hit::SessionItem(highlighted)),
                Key::Escape | Key::Tab | Key::BackTab => self.session_menu = None,
                _ => {}
            }
            return Vec::new();
        }
        match key {
            Key::Tab | Key::BackTab => {
                let order = self.focus_order();
                let at = order.iter().position(|f| *f == self.focus).unwrap_or(0);
                let next = if key == Key::Tab { (at + 1) % order.len() } else { (at + order.len() - 1) % order.len() };
                self.focus = order[next];
                return Vec::new();
            }
            Key::Escape => {
                if self.confirming.take().is_some() {
                    self.focus = Focus::Main;
                    return Vec::new();
                }
                return self.cancel_login();
            }
            _ => {}
        }
        let activate = matches!(&key, Key::Return) || matches!(&key, Key::Text(t) if t == " ");
        match self.focus {
            Focus::Session => {
                if activate || matches!(key, Key::Up | Key::Down) {
                    return self.activate(Hit::SessionButton);
                }
                Vec::new()
            }
            Focus::Power(n) => {
                if activate {
                    if let Some(hit) = self.power_view().hits().get(n).copied() {
                        return self.activate(hit);
                    }
                }
                Vec::new()
            }
            Focus::Main => self.main_key(key),
        }
    }

    fn main_key(&mut self, key: Key) -> Vec<Effect> {
        if self.login.question().is_some() {
            match key {
                // The answer stays shown while it is checked.
                Key::Return => return self.login.answer(&self.answer).map(Effect::Greetd).into_iter().collect(),
                Key::Backspace => {
                    self.answer.pop();
                }
                Key::Text(t) => self.answer.push_str(&t),
                _ => {}
            }
            return Vec::new();
        }
        if !self.login.idle() {
            // Waiting for greetd.
            return Vec::new();
        }
        if self.ask_username() {
            match key {
                Key::Return => return self.begin_selected(),
                Key::Backspace => {
                    self.username.pop();
                }
                Key::Text(t) => self.username.push_str(&t),
                _ => {}
            }
            return Vec::new();
        }
        let count = self.list().len();
        match key {
            Key::Up => self.selected = self.selected.saturating_sub(1),
            Key::Down => self.selected = (self.selected + 1).min(count.saturating_sub(1)),
            Key::Return => return self.begin_selected(),
            Key::Text(t) if t == " " => return self.begin_selected(),
            _ => {}
        }
        Vec::new()
    }

    /// greetd's reply to the last request.
    pub fn greetd_reply(&mut self, response: Response) -> Vec<Effect> {
        if matches!(response, Response::Error { .. }) {
            self.answer.clear();
        }
        let step = self.login.reply(response);
        let effects = match step {
            Step::Send(request) => vec![Effect::Greetd(request)],
            Step::Wait => Vec::new(),
            Step::Authenticated => {
                let request = match self.sessions.get(self.session) {
                    Some(session) => self.login.start(sessions::start_request(session)),
                    None => self.login.abandon("No sessions are installed."),
                };
                request.map(Effect::Greetd).into_iter().collect()
            }
            Step::Started => {
                let state = State { last_user: self.login.user().map(str::to_owned), last_session: self.sessions.get(self.session).map(|s| s.id.clone()) };
                vec![Effect::Finished(state)]
            }
        };
        self.after_login_change();
        if self.login.idle() {
            // Back at the list, on the user who tried.
            self.focus = Focus::Main;
        }
        effects
    }

    pub fn view(&self, now: i64) -> View {
        let main = if !self.login.idle() {
            Main::Login { user: self.login_display.clone(), question: self.question.clone(), text: self.answer.clone(), busy: self.login.busy() || self.login.question().is_none() }
        } else if self.ask_username() {
            Main::Username { text: self.username.clone() }
        } else if self.users.is_none() {
            Main::Loading
        } else {
            Main::Users { names: self.list().iter().map(|u| u.display.clone()).collect(), selected: self.selected }
        };
        View {
            time: crate::clock::time(now),
            date: crate::clock::date(now),
            banner: self.config.banner.clone(),
            main,
            notes: self.login.notes().to_vec(),
            sessions: self.sessions.iter().map(|s| s.name.clone()).collect(),
            session: self.session,
            session_menu: self.session_menu,
            power: self.power_view(),
            focus: self.focus,
            hover: self.hover,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::greetd::{AuthMessageType, ErrorType};
    use crate::login::RETRY_MESSAGE;

    fn session(id: &str, name: &str) -> Session {
        Session { id: id.into(), name: name.into(), exec: format!("{id}-session"), desktop_names: None }
    }

    fn user(name: &str, display: &str) -> User {
        User { name: name.into(), display: display.into(), login_time: 0 }
    }

    fn greeter() -> Greeter {
        let state = State { last_user: Some("bob".into()), last_session: None };
        let mut g = Greeter::new(Config::default(), vec![session("helix", "Helix"), session("sway", "Sway")], &state);
        g.set_users(vec![user("ann", "Ann"), user("bob", "Bob")]);
        g
    }

    fn secret() -> Response {
        Response::AuthMessage { auth_message_type: AuthMessageType::Secret, auth_message: "Password:".into() }
    }

    fn typed(g: &mut Greeter, s: &str) {
        for c in s.chars() {
            g.key(Key::Text(c.to_string()));
        }
    }

    #[test]
    fn last_user_selected_and_login() {
        let mut g = greeter();
        assert_eq!(g.view(0).main, Main::Users { names: vec!["Ann".into(), "Bob".into()], selected: 1 });
        assert_eq!(g.key(Key::Return), vec![Effect::Greetd(Request::CreateSession { username: "bob".into() })]);
        assert!(matches!(g.view(0).main, Main::Login { busy: true, question: None, .. }));
        assert!(g.key(Key::Text("x".into())).is_empty(), "no typing while waiting");
        g.greetd_reply(secret());
        typed(&mut g, "pw!");
        g.key(Key::Backspace);
        typed(&mut g, "x");
        assert!(matches!(&g.view(0).main, Main::Login { text, busy: false, .. } if text == "pwx"));
        assert_eq!(g.key(Key::Return), vec![Effect::Greetd(Request::PostAuthMessageResponse { response: Some("pwx".into()) })]);
        assert!(matches!(&g.view(0).main, Main::Login { text, busy: true, .. } if text.chars().count() == 3), "dots kept while checking");
        let effects = g.greetd_reply(Response::Success);
        assert_eq!(effects, vec![Effect::Greetd(sessions::start_request(&session("helix", "Helix")))]);
        assert_eq!(g.greetd_reply(Response::Success), vec![Effect::Finished(State { last_user: Some("bob".into()), last_session: Some("helix".into()) })]);
    }

    #[test]
    fn wrong_password_then_escape() {
        let mut g = greeter();
        g.key(Key::Up);
        g.key(Key::Return);
        g.greetd_reply(secret());
        typed(&mut g, "bad");
        g.key(Key::Return);
        assert_eq!(g.greetd_reply(Response::Error { error_type: ErrorType::AuthError, description: "x".into() }), vec![Effect::Greetd(Request::CancelSession)]);
        assert_eq!(g.greetd_reply(Response::Success), vec![Effect::Greetd(Request::CreateSession { username: "ann".into() })]);
        g.greetd_reply(secret());
        let view = g.view(0);
        assert_eq!(view.notes[0].text, RETRY_MESSAGE);
        assert!(matches!(&view.main, Main::Login { text, busy: false, user, .. } if text.is_empty() && user == "Ann"));
        assert_eq!(g.key(Key::Escape), vec![Effect::Greetd(Request::CancelSession)]);
        g.greetd_reply(Response::Success);
        assert_eq!(g.view(0).main, Main::Users { names: vec!["Ann".into(), "Bob".into()], selected: 0 });
    }

    #[test]
    fn session_menu_and_tab() {
        let mut g = greeter();
        g.key(Key::Tab);
        assert_eq!(g.focus, Focus::Session);
        g.key(Key::Return);
        assert_eq!(g.view(0).session_menu, Some(0));
        g.key(Key::Down);
        g.key(Key::Return);
        assert_eq!((g.view(0).session, g.view(0).session_menu), (1, None));
        g.key(Key::Tab);
        assert_eq!(g.focus, Focus::Main, "no power buttons: back to the list");
        g.set_power(Offered { restart: true, power_off: true });
        g.key(Key::BackTab);
        assert_eq!(g.focus, Focus::Power(1));
        // Clicks.
        g.click(Some(Hit::SessionButton));
        assert!(g.view(0).session_menu.is_some());
        g.click(None);
        assert!(g.view(0).session_menu.is_none(), "a click elsewhere closes it");
        g.click(Some(Hit::SessionButton));
        g.click(Some(Hit::SessionItem(0)));
        assert_eq!(g.view(0).session, 0);
        // Logging in with the chosen session.
        g.click(Some(Hit::User(0)));
        g.greetd_reply(Response::Success);
        g.click(Some(Hit::SessionButton));
        g.click(Some(Hit::SessionItem(1)));
        assert!(g.view(0).session_menu.is_none());
    }

    #[test]
    fn power_confirmation() {
        let mut g = greeter();
        g.set_power(Offered { restart: true, power_off: false });
        assert_eq!(g.view(0).power.hits(), vec![Hit::Power(PowerAction::Restart)]);
        g.key(Key::BackTab);
        assert_eq!(g.focus, Focus::Power(0));
        assert!(g.key(Key::Return).is_empty());
        assert_eq!(g.view(0).power, PowerView::Confirm(PowerAction::Restart));
        assert_eq!(g.key(Key::Return), vec![Effect::Power(PowerAction::Restart)]);
        g.click(Some(Hit::Power(PowerAction::Restart)));
        assert!(g.key(Key::Escape).is_empty());
        assert_eq!(g.view(0).power, PowerView::Buttons(Offered { restart: true, power_off: false }));
        g.click(Some(Hit::Power(PowerAction::Restart)));
        g.click(Some(Hit::PowerCancel));
        assert!(matches!(g.view(0).power, PowerView::Buttons(_)));
    }

    #[test]
    fn username_field() {
        let config = Config { user_list: false, banner: Some("Hi".into()) };
        let mut g = Greeter::new(config, vec![session("helix", "Helix")], &State::default());
        g.set_users(vec![user("ann", "Ann")]);
        assert_eq!(g.view(0).main, Main::Username { text: String::new() });
        assert!(g.key(Key::Return).is_empty(), "no name yet");
        typed(&mut g, "carol");
        assert_eq!(g.key(Key::Return), vec![Effect::Greetd(Request::CreateSession { username: "carol".into() })]);
        // No users found: the field too.
        let mut g = Greeter::new(Config::default(), vec![], &State::default());
        assert_eq!(g.view(0).main, Main::Loading);
        g.set_users(vec![]);
        assert!(matches!(g.view(0).main, Main::Username { .. }));
        // No sessions: authentication succeeds but nothing starts.
        typed(&mut g, "dan");
        g.key(Key::Return);
        assert_eq!(g.greetd_reply(Response::Success), vec![Effect::Greetd(Request::CancelSession)]);
        g.greetd_reply(Response::Success);
        assert_eq!(g.view(0).notes[0].text, "No sessions are installed.");
    }
}
