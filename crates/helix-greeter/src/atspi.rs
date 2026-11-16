//! The login screen on the accessibility bus (helix-desktop-spec
//! `components/greeter.md`, "Accessibility"; `contracts/dbus/org.a11y.atspi.md`):
//! one window, "Login screen", described from the same [`View`] the
//! painter draws and the places its last paint left ([`Hits`]). An action
//! on a node (Click, Focus on a field) is a click at the node's place, so
//! it does what a click does.

use helix_a11y::publish::Event;
use helix_a11y::tree::Target;
use helix_a11y::{Node, Rect as A11yRect, Role, Window};
use helix_i18n::tr;

use crate::app::{Focus, Hit, Main, PowerView, Setting, View};
use crate::osk::{self, Action};
use crate::paint::Hits;
use crate::power::PowerAction;

/// The window's key in the publisher (the greeter has one).
pub const KEY: &str = "login";

/// The window's name.
pub fn title() -> &'static str {
    tr("Login screen")
}

/// Where `hit` was painted last.
fn at(hits: &Hits, hit: Hit) -> Option<A11yRect> {
    hits.iter().rev().find(|(_, h)| *h == hit).map(|(r, _)| A11yRect::new(r.x, r.y, r.w, r.h))
}

/// `node`, placed and clickable where `hit` was painted (shown nodes
/// only: a user scrolled out of view cannot be clicked).
fn placed(node: Node, hits: &Hits, hit: Hit) -> Node {
    match at(hits, hit) {
        Some(r) => node.at(r).click(),
        None => node,
    }
}

fn button(name: impl Into<String>, hits: &Hits, hit: Hit) -> Node {
    placed(Node::new(Role::Button, name), hits, hit)
}

/// The login screen as the window `w`×`h` (its surface's size).
pub fn window(view: &View, hits: &Hits, (w, h): (i32, i32)) -> Window {
    let menu_open = view.session_menu.is_some() || view.a11y_menu.is_some();
    let focus = |f: Focus| view.focus == f && !menu_open;
    let mut children = vec![Node::label(view.time.clone()).id("time"), Node::label(view.date.clone()).id("date")];
    if let Some(banner) = &view.banner {
        children.push(Node::label(banner.clone()).id("banner"));
    }
    match &view.main {
        Main::Loading => {}
        Main::Users { names, selected, .. } => {
            let items = names.iter().enumerate().map(|(i, name)| {
                let item = Node::new(Role::ListItem, name.clone()).id(format!("user:{i}")).selected(i == *selected).focusable();
                placed(item, hits, Hit::User(i)).focused(focus(Focus::Main) && i == *selected)
            });
            children.push(Node::new(Role::List, tr("Users")).id("users").children(items));
        }
        Main::Username { text } => {
            let r = at(hits, Hit::Field).unwrap_or_default();
            children.push(Node::text_field(Role::TextInput, tr("Username"), text.clone(), r).id("username").focused(focus(Focus::Main)));
        }
        Main::Login { user, question, text, busy, .. } => {
            children.push(Node::label(user.clone()).id("user"));
            if let Some(q) = question {
                let r = at(hits, Hit::Field).unwrap_or_default();
                let field = if q.secret { Node::text_field(Role::PasswordInput, q.prompt.clone(), Node::mask(text), r) } else { Node::text_field(Role::TextInput, q.prompt.clone(), text.clone(), r) };
                // Waiting for greetd: the field takes no typing.
                children.push(field.id("answer").disabled(*busy).focused(focus(Focus::Main) && !busy));
            }
        }
    }
    for (i, note) in view.notes.iter().enumerate() {
        children.push(Node::label(note.text.clone()).id(format!("note:{i}")).live());
    }
    if let Main::Login { .. } = view.main {
        children.push(button(tr("Back"), hits, Hit::Back).id("back"));
    }
    if let Some(osk) = &view.osk {
        let keys = osk.keys().into_iter().enumerate().map(|(n, k)| {
            let key = button(osk::name(k.action), hits, Hit::OskKey(n)).id(format!("key:{n}"));
            if k.action == Action::Shift { key.toggled(osk.shift) } else { key }
        });
        children.push(Node::new(Role::Group, tr("Screen Keyboard")).id("osk").children(keys));
    }
    match view.power {
        PowerView::Buttons(_) => {
            for (n, hit) in view.power.hits().into_iter().enumerate() {
                let name = if hit == Hit::Power(PowerAction::Restart) { tr("Restart…") } else { tr("Power Off…") };
                children.push(button(name, hits, hit).focused(focus(Focus::Power(n))));
            }
        }
        PowerView::Confirm(action) => {
            let (question, verb) = match action {
                PowerAction::Restart => (tr("Restart this computer?"), tr("Restart")),
                PowerAction::PowerOff => (tr("Power off this computer?"), tr("Power Off")),
            };
            children.push(Node::label(question).id("power-question").live());
            children.push(button(tr("Cancel"), hits, Hit::PowerCancel).focused(focus(Focus::Power(0))));
            children.push(button(verb, hits, Hit::PowerConfirm).focused(focus(Focus::Power(1))));
        }
    }
    if let Some(name) = view.sessions.get(view.session) {
        let open = view.session_menu.is_some();
        children.push(button(name.clone(), hits, Hit::SessionButton).id("session").describe(tr("Session")).expanded(open).focused(focus(Focus::Session)));
        if let Some(highlighted) = view.session_menu {
            let items = view.sessions.iter().enumerate().map(|(i, s)| placed(Node::new(Role::MenuItemRadio, s.clone()).id(format!("session:{i}")), hits, Hit::SessionItem(i)).toggled(i == view.session).focused(i == highlighted));
            children.push(Node::new(Role::Menu, tr("Session")).id("session-menu").children(items));
        }
    }
    children.push(button(tr("Accessibility"), hits, Hit::A11yButton).id("a11y").expanded(view.a11y_menu.is_some()).focused(focus(Focus::A11y)));
    if let Some(highlighted) = view.a11y_menu {
        let items = Setting::ALL.iter().enumerate().map(|(i, s)| placed(Node::new(Role::MenuItemCheckBox, s.name()), hits, Hit::A11yItem(*s)).toggled(s.get(&view.accessibility)).focused(i == highlighted));
        children.push(Node::new(Role::Menu, tr("Accessibility")).id("a11y-menu").children(items));
    }
    Window { title: title().into(), bounds: A11yRect::new(0, 0, w, h), active: true, children }
}

/// What an action from the accessibility bus clicks: the target painted
/// at the node's place.
pub fn hit_for(hits: &Hits, target: &Target, request: &helix_a11y::accesskit::ActionRequest) -> Option<Hit> {
    let (x, y) = helix_a11y::tree::click_point(target, request.action, request.data.as_ref())?;
    crate::paint::hit_at(hits, x, y)
}

/// Where the adapters' events go (from their thread).
pub type Notify = helix_a11y::publish::Notify;

/// A notifier sending each event through `send` (a channel's sender,
/// which need not be `Sync`: it is kept behind a lock).
pub fn notify<S: Send + 'static>(sender: S, send: fn(&S, Event)) -> Notify {
    let sender = std::sync::Mutex::new(sender);
    std::sync::Arc::new(move |event| {
        if let Ok(s) = sender.lock() {
            send(&s, event);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::View;
    use crate::login::{Note, Question};
    use crate::paint::Rect;
    use crate::power::Offered;

    fn view(main: Main) -> View {
        View {
            time: "14:05".into(),
            date: "Thursday".into(),
            banner: None,
            main,
            notes: vec![Note { text: "Sorry".into(), error: true }],
            sessions: vec!["Helix".into(), "Sway".into()],
            session: 0,
            session_menu: None,
            power: PowerView::Buttons(Offered { restart: true, power_off: false }),
            focus: Focus::Main,
            hover: None,
            accessibility: Default::default(),
            a11y_menu: None,
            osk: None,
        }
    }

    #[test]
    fn describes_the_screen() {
        let hits: Hits = vec![(Rect { x: 10, y: 10, w: 100, h: 40 }, Hit::Field), (Rect { x: 0, y: 700, w: 80, h: 36 }, Hit::Power(PowerAction::Restart)), (Rect { x: 1000, y: 700, w: 36, h: 36 }, Hit::A11yButton)];
        let v = view(Main::Login { user: "Ann".into(), login: "ann".into(), question: Some(Question { prompt: "Password:".into(), secret: true }), text: "hunter2".into(), busy: false });
        let w = window(&v, &hits, (1280, 800));
        assert_eq!(w.title, "Login screen");
        let field = w.find(|n| n.role == Role::PasswordInput).expect("the field");
        assert_eq!((field.name.as_str(), field.text.as_deref()), ("Password:", Some("•••••••")));
        assert_eq!(w.focus().map(|n| n.name.as_str()), Some("Password:"));
        assert!(w.find(|n| n.text.as_deref() == Some("Sorry") && n.live).is_some());
        let restart = w.find(|n| n.name == "Restart…").unwrap();
        assert!(restart.clickable && restart.bounds == Some(A11yRect::new(0, 700, 80, 36)));
        let back = w.find(|n| n.name == "Back").unwrap();
        assert!(!back.clickable, "not painted: nothing to click");
        assert_eq!(w.find(|n| n.name == "Accessibility").unwrap().expanded, Some(false));
        // A visible question, busy: a plain field that takes no typing.
        let v = view(Main::Login { user: "Ann".into(), login: "ann".into(), question: Some(Question { prompt: "Token:".into(), secret: false }), text: "12".into(), busy: true });
        let w = window(&v, &hits, (1280, 800));
        let field = w.find(|n| n.role == Role::TextInput).unwrap();
        assert!(field.disabled && !field.focused && field.text.as_deref() == Some("12"));
    }

    #[test]
    fn menus_and_keyboard() {
        let mut v = view(Main::Users { names: vec!["Ann".into(), "Bob".into()], logins: vec!["ann".into(), "bob".into()], selected: 1 });
        v.a11y_menu = Some(1);
        v.focus = Focus::A11y;
        v.accessibility.large_text = true;
        v.osk = Some(crate::osk::Osk { page: crate::osk::Page::Letters, shift: true });
        let hits: Hits = vec![(Rect { x: 10, y: 10, w: 100, h: 40 }, Hit::User(1)), (Rect { x: 1, y: 1, w: 2, h: 2 }, Hit::A11yItem(Setting::LargeText))];
        let w = window(&v, &hits, (1280, 800));
        let list = w.find(|n| n.role == Role::List).unwrap();
        assert_eq!(list.children.len(), 2);
        assert_eq!(list.children[1].selected, Some(true));
        assert!(list.children[1].clickable && !list.children[0].clickable);
        assert!(!list.children[1].focused, "the menu has the focus");
        let menu = w.find(|n| n.role == Role::Menu).unwrap();
        let names: Vec<&str> = menu.children.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["High Contrast", "Large Text", "Screen Reader", "Screen Keyboard"]);
        assert_eq!(menu.children[1].toggled, Some(true));
        assert_eq!(menu.children[0].toggled, Some(false));
        assert_eq!(w.focus().map(|n| n.name.as_str()), Some("Large Text"));
        let osk = w.find(|n| n.name == "Screen Keyboard" && n.role == Role::Group).unwrap();
        assert_eq!(osk.children[0].name, "Q");
        assert_eq!(osk.children.iter().find(|n| n.name == "Shift").unwrap().toggled, Some(true));
        // An action clicks what is painted at the node's place.
        let built = helix_a11y::tree::build(&w);
        let (id, target) = built.targets.iter().find(|(_, t)| t.bounds == A11yRect::new(10, 10, 100, 40)).unwrap();
        let request = helix_a11y::accesskit::ActionRequest { action: helix_a11y::accesskit::Action::Click, target_tree: helix_a11y::accesskit::TreeId::ROOT, target_node: *id, data: None };
        assert_eq!(hit_for(&hits, target, &request), Some(Hit::User(1)));
    }
}
