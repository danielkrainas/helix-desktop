//! The greeter's state and input handling, apart from Wayland: keys and
//! clicks in, greetd requests and power actions out, and the view the
//! painter draws (helixdesktop/spec `components/greeter.md`).

use crate::config::Config;
use crate::greetd::{Request, Response};
use crate::login::{Login, Note, Question, Step};
use crate::osk::Osk;
use crate::power::{Offered, PowerAction};
use crate::sessions::{self, Session};
use crate::state::{Accessibility, State};
use crate::users::User;
use helix_i18n::tr;

/// A setting in the accessibility menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    HighContrast,
    LargeText,
    ScreenReader,
    ScreenKeyboard,
}

impl Setting {
    /// The menu's items, in order.
    pub const ALL: [Setting; 4] = [Setting::HighContrast, Setting::LargeText, Setting::ScreenReader, Setting::ScreenKeyboard];

    pub fn name(self) -> &'static str {
        match self {
            Self::HighContrast => tr("High Contrast"),
            Self::LargeText => tr("Large Text"),
            Self::ScreenReader => tr("Screen Reader"),
            Self::ScreenKeyboard => tr("Screen Keyboard"),
        }
    }

    pub fn get(self, a: &Accessibility) -> bool {
        match self {
            Self::HighContrast => a.high_contrast,
            Self::LargeText => a.large_text,
            Self::ScreenReader => a.screen_reader,
            Self::ScreenKeyboard => a.screen_keyboard,
        }
    }

    fn slot(self, a: &mut Accessibility) -> &mut bool {
        match self {
            Self::HighContrast => &mut a.high_contrast,
            Self::LargeText => &mut a.large_text,
            Self::ScreenReader => &mut a.screen_reader,
            Self::ScreenKeyboard => &mut a.screen_keyboard,
        }
    }
}

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
    /// Alt+Super+S: the screen reader on or off.
    ScreenReader,
}

/// What has keyboard focus (Tab cycles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The user list or the field.
    Main,
    Session,
    /// The accessibility button.
    A11y,
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
    A11yButton,
    A11yItem(Setting),
    /// The n-th key of the on-screen keyboard ([`Osk::keys`]).
    OskKey(usize),
}

/// What the caller does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Greetd(Request),
    Power(PowerAction),
    /// The accessibility settings changed: apply `state.accessibility`
    /// (high contrast, the text scale, the screen reader) and save `state`.
    Accessibility(State),
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
    /// Display names, login names (for their pictures), the selection.
    Users { names: Vec<String>, logins: Vec<String>, selected: usize },
    Username { text: String },
    Login {
        user: String,
        /// The login name (for the picture).
        login: String,
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
    pub accessibility: Accessibility,
    /// The accessibility menu, open, with its highlighted item.
    pub a11y_menu: Option<usize>,
    /// The on-screen keyboard, when shown.
    pub osk: Option<Osk>,
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
    /// What the greeter remembered (each user's last session).
    state: State,
    /// The session was picked from the menu: choosing a user keeps it.
    session_picked: bool,
    a11y_menu: Option<usize>,
    osk: Osk,
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
            state: state.clone(),
            session_picked: false,
            a11y_menu: None,
            osk: Osk::default(),
            focus: Focus::Main,
            hover: None,
        }
    }

    /// The accessibility settings in effect.
    pub fn accessibility(&self) -> Accessibility {
        self.state.accessibility
    }

    /// Turn `setting` on or off.
    fn toggle(&mut self, setting: Setting) -> Vec<Effect> {
        let slot = setting.slot(&mut self.state.accessibility);
        *slot = !*slot;
        if setting == Setting::ScreenKeyboard {
            self.osk = Osk::default();
        }
        vec![Effect::Accessibility(self.state.clone())]
    }

    /// The users AccountsService or `/etc/passwd` gave; the last user to
    /// log in is selected.
    pub fn set_users(&mut self, users: Vec<User>) {
        self.selected = self.last_user.as_deref().and_then(|last| users.iter().position(|u| u.name == last)).unwrap_or(0);
        self.users = Some(users);
        self.follow_user();
    }

    /// The session menu follows the selected user (unless a session was
    /// picked from it): their last session through the greeter, else the
    /// one AccountsService remembers for them, else the last one used,
    /// else Helix, else the first.
    fn follow_user(&mut self) {
        if self.session_picked {
            return;
        }
        let user = self.list().get(self.selected);
        let remembered = user.and_then(|u| self.state.sessions.get(&u.name)).map(String::as_str);
        let accounts = user.and_then(|u| u.session.as_deref());
        let installed = |id: &str| self.sessions.iter().any(|s| s.id == id);
        let choice = remembered.filter(|id| installed(id)).or(accounts.filter(|id| installed(id))).or(self.state.last_session.as_deref());
        self.session = sessions::default_index(&self.sessions, choice);
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

    /// The focus cycle: main, session button, accessibility button, power
    /// buttons.
    fn focus_order(&self) -> Vec<Focus> {
        let mut order = vec![Focus::Main];
        if !self.sessions.is_empty() {
            order.push(Focus::Session);
        }
        order.push(Focus::A11y);
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
                    self.follow_user();
                    return self.begin_selected();
                }
            }
            Hit::Field => self.focus = Focus::Main,
            Hit::Back => return self.cancel_login(),
            Hit::SessionButton => {
                self.focus = Focus::Session;
                self.a11y_menu = None;
                self.session_menu = if self.session_menu.is_some() || self.sessions.is_empty() { None } else { Some(self.session) };
            }
            Hit::A11yButton => {
                self.focus = Focus::A11y;
                self.session_menu = None;
                self.a11y_menu = if self.a11y_menu.is_some() { None } else { Some(0) };
            }
            Hit::A11yItem(setting) => {
                // The menu stays open: several settings can be changed.
                self.focus = Focus::A11y;
                self.a11y_menu = Setting::ALL.iter().position(|s| *s == setting);
                return self.toggle(setting);
            }
            Hit::OskKey(n) => {
                // Typing goes to the list or the field.
                let Some(key) = self.osk.press(n) else { return Vec::new() };
                self.focus = Focus::Main;
                return self.main_key(key);
            }
            Hit::SessionItem(i) => {
                if i < self.sessions.len() {
                    self.session = i;
                    self.session_picked = true;
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
        if self.a11y_menu.is_some() && !matches!(hit, Some(Hit::A11yItem(_) | Hit::A11yButton)) {
            self.a11y_menu = None;
            return Vec::new();
        }
        if matches!(hit, Some(Hit::OskKey(_))) && !self.state.accessibility.screen_keyboard {
            return Vec::new();
        }
        match hit {
            Some(hit) => self.activate(hit),
            None => Vec::new(),
        }
    }

    pub fn key(&mut self, key: Key) -> Vec<Effect> {
        if key == Key::ScreenReader {
            return self.toggle(Setting::ScreenReader);
        }
        if let Some(highlighted) = self.a11y_menu {
            let last = Setting::ALL.len() - 1;
            match key {
                Key::Up => self.a11y_menu = Some(highlighted.saturating_sub(1)),
                Key::Down => self.a11y_menu = Some((highlighted + 1).min(last)),
                Key::Return => return self.activate(Hit::A11yItem(Setting::ALL[highlighted])),
                Key::Text(t) if t == " " => return self.activate(Hit::A11yItem(Setting::ALL[highlighted])),
                Key::Escape | Key::Tab | Key::BackTab => self.a11y_menu = None,
                _ => {}
            }
            return Vec::new();
        }
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
            Focus::A11y => {
                if activate || matches!(key, Key::Up | Key::Down) {
                    return self.activate(Hit::A11yButton);
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
            Key::Up => {
                self.selected = self.selected.saturating_sub(1);
                self.follow_user();
            }
            Key::Down => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1));
                self.follow_user();
            }
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
                    None => self.login.abandon(tr("No sessions are installed.")),
                };
                request.map(Effect::Greetd).into_iter().collect()
            }
            Step::Started => {
                let mut state = self.state.clone();
                state.last_user = self.login.user().map(str::to_owned);
                state.last_session = self.sessions.get(self.session).map(|s| s.id.clone());
                if let (Some(user), Some(session)) = (&state.last_user, &state.last_session) {
                    state.sessions.insert(user.clone(), session.clone());
                }
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
            Main::Login { user: self.login_display.clone(), login: self.login.user().unwrap_or_default().to_owned(), question: self.question.clone(), text: self.answer.clone(), busy: self.login.busy() || self.login.question().is_none() }
        } else if self.ask_username() {
            Main::Username { text: self.username.clone() }
        } else if self.users.is_none() {
            Main::Loading
        } else {
            Main::Users { names: self.list().iter().map(|u| u.display.clone()).collect(), logins: self.list().iter().map(|u| u.name.clone()).collect(), selected: self.selected }
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
            accessibility: self.state.accessibility,
            a11y_menu: self.a11y_menu,
            osk: self.state.accessibility.screen_keyboard.then_some(self.osk),
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
        User::new(name, display, 0)
    }

    fn greeter() -> Greeter {
        let state = State { last_user: Some("bob".into()), last_session: None, sessions: Default::default(), ..Default::default() };
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
        assert_eq!(g.view(0).main, Main::Users { names: vec!["Ann".into(), "Bob".into()], logins: vec!["ann".into(), "bob".into()], selected: 1 });
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
        assert_eq!(g.greetd_reply(Response::Success), vec![Effect::Finished(State { last_user: Some("bob".into()), last_session: Some("helix".into()), sessions: [("bob".to_owned(), "helix".to_owned())].into(), ..Default::default() })]);
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
        assert_eq!(g.view(0).main, Main::Users { names: vec!["Ann".into(), "Bob".into()], logins: vec!["ann".into(), "bob".into()], selected: 0 });
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
        assert_eq!(g.focus, Focus::A11y, "the accessibility button");
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
        let config = Config { user_list: false, banner: Some("Hi".into()), accent: None, clock_format: None };
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

    /// The session follows the selected user: their last one through the
    /// greeter, else AccountsService's, else the last used; a session
    /// picked from the menu stays.
    #[test]
    fn per_user_sessions() {
        let sessions = vec![session("helix", "Helix"), session("sway", "Sway"), session("gnome", "GNOME")];
        let state = State { last_user: Some("ann".into()), last_session: Some("gnome".into()), sessions: [("ann".to_owned(), "sway".to_owned())].into(), ..Default::default() };
        let mut g = Greeter::new(Config::default(), sessions, &state);
        let mut bob = user("bob", "Bob");
        bob.session = Some("helix".into());
        let mut carol = user("carol", "Carol");
        carol.session = Some("not-installed".into());
        g.set_users(vec![user("ann", "Ann"), bob, carol]);
        assert_eq!(g.view(0).session, 1, "Ann: her last session through the greeter");
        g.key(Key::Down);
        assert_eq!(g.view(0).session, 0, "Bob: AccountsService's");
        g.key(Key::Down);
        assert_eq!(g.view(0).session, 2, "Carol: not installed, so the last used");
        g.click(Some(Hit::SessionButton));
        g.click(Some(Hit::SessionItem(1)));
        g.focus = Focus::Main;
        g.key(Key::Up);
        assert_eq!(g.view(0).session, 1, "picked from the menu: kept");
        g.key(Key::Return);
        g.greetd_reply(secret());
        g.key(Key::Return);
        g.greetd_reply(Response::Success);
        let Some(Effect::Finished(saved)) = g.greetd_reply(Response::Success).pop() else { panic!("finished") };
        assert_eq!(saved.last_user.as_deref(), Some("bob"));
        assert_eq!(saved.sessions.get("bob").map(String::as_str), Some("sway"));
        assert_eq!(saved.sessions.get("ann").map(String::as_str), Some("sway"), "others kept");
    }

    fn settings(effects: &[Effect]) -> Accessibility {
        match effects {
            [Effect::Accessibility(state)] => state.accessibility,
            other => panic!("expected one Accessibility effect, got {other:?}"),
        }
    }

    /// The accessibility menu by keyboard: Tab to its button, Return opens
    /// it, arrows move, Return and Space toggle (it stays open), Escape
    /// closes it.
    #[test]
    fn accessibility_menu_by_keyboard() {
        let mut g = greeter();
        g.key(Key::Tab);
        g.key(Key::Tab);
        assert_eq!(g.focus, Focus::A11y);
        assert!(g.key(Key::Return).is_empty());
        assert_eq!(g.view(0).a11y_menu, Some(0));
        let a = settings(&g.key(Key::Return));
        assert!(a.high_contrast && !a.large_text);
        assert_eq!(g.view(0).a11y_menu, Some(0), "still open");
        assert!(g.view(0).accessibility.high_contrast);
        g.key(Key::Down);
        let a = settings(&g.key(Key::Text(" ".into())));
        assert!(a.high_contrast && a.large_text);
        g.key(Key::Down);
        g.key(Key::Down);
        g.key(Key::Down);
        assert_eq!(g.view(0).a11y_menu, Some(3), "stops at the last");
        let a = settings(&g.key(Key::Return));
        assert!(a.screen_keyboard);
        assert!(g.view(0).osk.is_some(), "the keyboard is shown");
        g.key(Key::Up);
        g.key(Key::Return);
        assert!(g.accessibility().screen_reader);
        assert!(g.key(Key::Escape).is_empty());
        assert_eq!(g.view(0).a11y_menu, None);
        assert_eq!(g.focus, Focus::A11y);
        // Typing does not reach the list while the menu is open.
        g.key(Key::Return);
        g.key(Key::Text("x".into()));
        assert_eq!(g.view(0).a11y_menu, Some(0));
        g.key(Key::Tab);
        assert_eq!(g.view(0).a11y_menu, None, "Tab closes it");
    }

    #[test]
    fn accessibility_menu_by_pointer_and_shortcut() {
        let mut g = greeter();
        g.click(Some(Hit::SessionButton));
        g.click(Some(Hit::A11yButton));
        assert_eq!((g.view(0).session_menu, g.view(0).a11y_menu), (None, None), "a click elsewhere closes the session menu first");
        g.click(Some(Hit::A11yButton));
        assert_eq!(g.view(0).a11y_menu, Some(0));
        assert!(settings(&g.click(Some(Hit::A11yItem(Setting::LargeText)))).large_text);
        assert_eq!(g.view(0).a11y_menu, Some(1), "the item clicked is highlighted");
        g.click(None);
        assert_eq!(g.view(0).a11y_menu, None);
        // Alt+Super+S, whatever has the focus and with the menu open.
        assert!(settings(&g.key(Key::ScreenReader)).screen_reader);
        g.click(Some(Hit::A11yButton));
        assert!(!settings(&g.key(Key::ScreenReader)).screen_reader);
        assert_eq!(g.view(0).a11y_menu, Some(0));
    }

    /// The settings are part of what is remembered, and come back.
    #[test]
    fn accessibility_is_remembered() {
        let mut g = greeter();
        g.click(Some(Hit::A11yButton));
        let Some(Effect::Accessibility(saved)) = g.click(Some(Hit::A11yItem(Setting::HighContrast))).pop() else { panic!("saved") };
        assert_eq!(saved.last_user.as_deref(), Some("bob"), "the rest of the state is kept");
        assert!(saved.accessibility.high_contrast);
        let g2 = Greeter::new(Config::default(), vec![session("helix", "Helix")], &saved);
        assert!(g2.accessibility().high_contrast && g2.view(0).accessibility.high_contrast);
        // A login keeps them too.
        g.click(None);
        g.focus = Focus::Main;
        g.key(Key::Return);
        g.greetd_reply(Response::Success);
        let Some(Effect::Finished(state)) = g.greetd_reply(Response::Success).pop() else { panic!("finished") };
        assert!(state.accessibility.high_contrast);
    }

    /// The on-screen keyboard types into the list and the field.
    #[test]
    fn screen_keyboard_types() {
        use crate::osk::Action;
        let mut g = greeter();
        let key = |g: &Greeter, action: Action| Hit::OskKey(g.view(0).osk.expect("shown").keys().iter().position(|k| k.action == action).unwrap());
        assert!(g.click(Some(Hit::OskKey(0))).is_empty(), "no keyboard: nothing");
        g.click(Some(Hit::A11yButton));
        g.click(Some(Hit::A11yItem(Setting::ScreenKeyboard)));
        g.click(None);
        g.focus = Focus::Session;
        // Return on the list begins the login, wherever the focus was.
        assert_eq!(g.click(Some(key(&g, Action::Return))), vec![Effect::Greetd(Request::CreateSession { username: "bob".into() })]);
        assert_eq!(g.focus, Focus::Main);
        g.greetd_reply(secret());
        g.click(Some(key(&g, Action::Shift)));
        g.click(Some(key(&g, Action::Char('P'))));
        g.click(Some(key(&g, Action::Char('w'))));
        g.click(Some(key(&g, Action::Char('x'))));
        g.click(Some(key(&g, Action::Backspace)));
        g.click(Some(key(&g, Action::Page(crate::osk::Page::Symbols))));
        g.click(Some(key(&g, Action::Char('1'))));
        g.click(Some(key(&g, Action::Space)));
        assert!(matches!(&g.view(0).main, Main::Login { text, .. } if text == "Pw1 "), "{:?}", g.view(0).main);
        assert_eq!(g.click(Some(key(&g, Action::Return))), vec![Effect::Greetd(Request::PostAuthMessageResponse { response: Some("Pw1 ".into()) })]);
        // Off again: the keyboard goes, back on its first page.
        g.click(Some(Hit::A11yButton));
        g.click(Some(Hit::A11yItem(Setting::ScreenKeyboard)));
        assert_eq!(g.view(0).osk, None);
        g.click(Some(Hit::A11yItem(Setting::ScreenKeyboard)));
        assert_eq!(g.view(0).osk, Some(Osk::default()));
    }
}
