//! `org.Gtk.MountOperationHandler` (helix-desktop-spec
//! `contracts/dbus/org.gtk.MountOperationHandler.md`): GTK apps' mount
//! operations (passwords for shares and encrypted disks, questions, apps
//! keeping a disk busy) ask the user through the shell's dialog.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};
use zbus::message::Header;
use zbus::zvariant::{OwnedValue, Value};
use zbus::interface;

use crate::prompt::{self, Answer, Prompt};
use crate::UiEvent;

pub const BUS_NAME: &str = "org.gtk.MountOperationHandler";
pub const PATH: &str = "/org/gtk/MountOperationHandler";

/// `GMountOperationResult`.
pub mod result {
    pub const HANDLED: u32 = 0;
    pub const ABORTED: u32 = 1;
    pub const UNHANDLED: u32 = 2;
}

/// `GAskPasswordFlags`.
pub mod flags {
    pub const NEED_PASSWORD: u32 = 1;
    pub const NEED_USERNAME: u32 = 2;
    pub const NEED_DOMAIN: u32 = 4;
    pub const SAVING_SUPPORTED: u32 = 8;
    pub const ANONYMOUS_SUPPORTED: u32 = 16;
    pub const TCRYPT: u32 = 32;
}

/// The extra answers of Helix's own mount password dialogs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Extras {
    pub anonymous: bool,
    pub hidden_volume: bool,
    pub system_volume: bool,
    pub pim: Option<u32>,
}

/// Adds what `flags` offer beyond a password: "Connect Anonymously"
/// (16); for VeraCrypt volumes (32) a "PIM" field (when there is no user
/// name field) and "Hidden Volume" / "Windows System Volume" boxes.
pub fn add_extras(prompt: &mut Prompt, flags: u32) {
    if flags & flags::ANONYMOUS_SUPPORTED != 0 {
        prompt.checks.push(("Connect Anonymously".into(), false));
    }
    if flags & flags::TCRYPT != 0 {
        if prompt.entry.is_none() {
            prompt.entry = Some(("PIM (optional)".into(), String::new()));
        }
        prompt.checks.push(("Hidden Volume".into(), false));
        prompt.checks.push(("Windows System Volume".into(), false));
    }
}

/// The extras answered, for a prompt made with [`add_extras`] (`user`:
/// whether the prompt had a user name or domain field of its own).
pub fn extras_of(flags: u32, user: bool, entry: Option<&str>, checks: &[bool]) -> Extras {
    let mut at = 0;
    let mut next = || {
        let v = checks.get(at).copied().unwrap_or(false);
        at += 1;
        v
    };
    let anonymous = flags & flags::ANONYMOUS_SUPPORTED != 0 && next();
    let tcrypt = flags & flags::TCRYPT != 0;
    let (hidden_volume, system_volume) = if tcrypt { (next(), next()) } else { (false, false) };
    let pim = if tcrypt && !user { entry.and_then(|e| e.trim().parse::<u32>().ok()) } else { None };
    Extras { anonymous, hidden_volume, system_volume, pim }
}

/// Adds a VeraCrypt volume's answers (`flags` has 32) to a password
/// answer: `hidden_volume` and `system_volume` (`b`) and `pim` (`u`, 0
/// for none), the keys GTK reads.
pub fn tcrypt_details(details: &mut Details, flags: u32, extras: &Extras) {
    if flags & flags::TCRYPT == 0 {
        return;
    }
    details.insert("hidden_volume".into(), value(Value::from(extras.hidden_volume)));
    details.insert("system_volume".into(), value(Value::from(extras.system_volume)));
    details.insert("pim".into(), value(Value::from(extras.pim.unwrap_or(0))));
}

/// `GPasswordSave`.
pub mod password_save {
    pub const NEVER: u32 = 0;
    pub const PERMANENTLY: u32 = 2;
}

pub type Details = HashMap<String, OwnedValue>;

fn value(v: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(v).expect("plain values convert")
}

/// A GIO message: its first line titles the dialog, the rest explains.
pub fn split_message(message: &str) -> (String, String) {
    match message.split_once('\n') {
        Some((head, rest)) => (head.trim().to_owned(), rest.trim().to_owned()),
        None => (message.trim().to_owned(), String::new()),
    }
}

/// What the user answered to a choice question: the choice, or `None`
/// (aborted).
pub type ChoiceMap = Box<dyn FnOnce(Answer) -> Option<i32> + Send>;

/// A question offering `choices`. Up to two choices are the dialog's
/// buttons (the first continues, the last cancels); more are rows to
/// pick from, with "Cancel" aborting.
pub fn choice_prompt(mut prompt: Prompt, choices: &[String]) -> Option<(Prompt, ChoiceMap)> {
    match choices.len() {
        0 => None,
        1 | 2 => {
            prompt.continue_label = choices[0].clone();
            let last = (choices.len() == 2).then(|| choices.len() as i32 - 1);
            prompt.cancel_label = if choices.len() == 2 { choices[1].clone() } else { "Cancel".into() };
            Some((
                prompt,
                Box::new(move |answer| match answer {
                    Answer::Continue { .. } => Some(0),
                    Answer::Cancel => last,
                }),
            ))
        }
        n => {
            prompt.list = choices.to_vec();
            prompt.selected = 0;
            prompt.activate_rows = true;
            prompt.continue_label = "Choose".into();
            prompt.cancel_label = "Cancel".into();
            Some((
                prompt,
                Box::new(move |answer| match answer {
                    Answer::Continue { selected, .. } => Some(selected.min(n - 1) as i32),
                    Answer::Cancel => None,
                }),
            ))
        }
    }
}

/// The password dialog for `flags`, or `None` when it would ask nothing.
pub fn password_prompt(message: &str, default_user: &str, default_domain: &str, flags: u32) -> Option<Prompt> {
    let need = |f: u32| flags & f != 0;
    if !need(flags::NEED_PASSWORD | flags::NEED_USERNAME | flags::NEED_DOMAIN) {
        return None;
    }
    let (title, rest) = split_message(message);
    let entry = match (need(flags::NEED_USERNAME), need(flags::NEED_DOMAIN)) {
        (true, true) => {
            let text = if default_domain.is_empty() { default_user.to_owned() } else { format!("{default_domain}\\{default_user}") };
            Some(("User name (domain\\name)".to_owned(), text))
        }
        (true, false) => Some(("User name".to_owned(), default_user.to_owned())),
        (false, true) => Some(("Domain".to_owned(), default_domain.to_owned())),
        (false, false) => None,
    };
    Some(Prompt {
        title: if title.is_empty() { "Password Required".into() } else { title },
        message: rest,
        entry,
        password: need(flags::NEED_PASSWORD).then(|| "Password".to_owned()),
        choice: need(flags::SAVING_SUPPORTED).then(|| ("Remember Password".to_owned(), remember_default())),
        continue_label: "Connect".into(),
        cancel_label: "Cancel".into(),
        ..Prompt::default()
    })
}

/// `org.gnome.shell remember-mount-password`: the "Remember Password"
/// box's first state.
pub fn remember_default() -> bool {
    use gio::prelude::*;
    helix_settings::has_schema("org.gnome.shell") && {
        let s = gio::Settings::new("org.gnome.shell");
        s.settings_schema().is_some_and(|x| x.has_key("remember-mount-password")) && s.boolean("remember-mount-password")
    }
}

/// Keeps the user's last "Remember Password" choice as the next default.
pub fn remember_chosen(on: bool) {
    use gio::prelude::*;
    if helix_settings::has_schema("org.gnome.shell") {
        let s = gio::Settings::new("org.gnome.shell");
        if s.settings_schema().is_some_and(|x| x.has_key("remember-mount-password")) && s.boolean("remember-mount-password") != on {
            let _ = s.set_boolean("remember-mount-password", on);
        }
    }
}

/// The `response_details` of an answered password dialog.
pub fn password_details(flags: u32, _default_user: &str, default_domain: &str, entry: Option<&str>, password: Option<&str>, remember: bool) -> Details {
    let need = |f: u32| flags & f != 0;
    let mut details = Details::new();
    let entry = entry.unwrap_or_default().trim();
    match (need(flags::NEED_USERNAME), need(flags::NEED_DOMAIN)) {
        (true, true) => {
            let (domain, user) = entry.split_once('\\').unwrap_or((default_domain, entry));
            details.insert("username".into(), value(Value::from(user.to_owned())));
            details.insert("domain".into(), value(Value::from(domain.to_owned())));
        }
        (true, false) => {
            details.insert("username".into(), value(Value::from(entry.to_owned())));
        }
        (false, true) => {
            details.insert("domain".into(), value(Value::from(entry.to_owned())));
        }
        (false, false) => {}
    }
    if need(flags::NEED_PASSWORD) {
        details.insert("password".into(), value(Value::from(password.unwrap_or_default().to_owned())));
    }
    let save = if need(flags::SAVING_SUPPORTED) && remember { password_save::PERMANENTLY } else { password_save::NEVER };
    details.insert("password_save".into(), value(Value::from(save)));
    details
}

/// Why a dialog went away without an answer.
enum Stop {
    /// The caller called `Close`.
    Closed,
    /// The same operation asked again: the dialog stays for the new call.
    Superseded,
}

struct Current {
    sender: String,
    id: String,
    token: u64,
    stop: oneshot::Sender<Stop>,
}

pub struct MountHandler {
    ui: mpsc::UnboundedSender<UiEvent>,
    current: Arc<Mutex<Option<Current>>>,
    next: std::sync::atomic::AtomicU64,
}

impl MountHandler {
    pub fn new(ui: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self { ui, current: Arc::default(), next: Default::default() }
    }

    fn current(&self) -> std::sync::MutexGuard<'_, Option<Current>> {
        self.current.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Show `prompt` for operation `id` of `sender` and map the answer.
    async fn run(&self, sender: String, id: String, prompt: Prompt, map: impl FnOnce(Answer) -> (u32, Details)) -> (u32, Details) {
        // The same operation asking again replaces its earlier call.
        let superseded = {
            let mut current = self.current();
            match current.as_ref() {
                Some(c) if c.sender == sender && c.id == id => current.take(),
                _ => None,
            }
        };
        if let Some(old) = superseded {
            let _ = old.stop.send(Stop::Superseded);
        }
        let _turn = prompt::turn().await;
        let token = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (stop_tx, stop_rx) = oneshot::channel();
        *self.current() = Some(Current { sender, id, token, stop: stop_tx });
        let outcome = tokio::select! {
            answer = prompt::ask(&self.ui, prompt) => Ok(answer),
            stop = stop_rx => Err(stop),
        };
        {
            let mut current = self.current();
            if current.as_ref().is_some_and(|c| c.token == token) {
                *current = None;
            }
        }
        match outcome {
            Ok(answer) => {
                prompt::done(&self.ui);
                map(answer)
            }
            // Left up for the operation's new call.
            Err(Ok(Stop::Superseded)) => (result::UNHANDLED, Details::new()),
            Err(_) => {
                prompt::done(&self.ui);
                (result::UNHANDLED, Details::new())
            }
        }
    }

    async fn question(&self, sender: String, id: String, prompt: Prompt, choices: &[String]) -> (u32, Details) {
        let Some((prompt, map)) = choice_prompt(prompt, choices) else {
            return (result::UNHANDLED, Details::new());
        };
        self.run(sender, id, prompt, move |answer| match map(answer) {
            Some(choice) => (result::HANDLED, Details::from([("choice".to_owned(), value(Value::from(choice)))])),
            None => (result::ABORTED, Details::new()),
        })
        .await
    }
}

/// The dialog's icon: theme icon `name` (none when empty or not found),
/// loaded at twice its 48-pixel size for scaled monitors.
fn icon(name: &str) -> Option<std::sync::Arc<helix_draw::Image>> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    crate::icons::load(&crate::icons::IconSource::Names(vec![name.to_owned()]), 96).map(std::sync::Arc::new)
}

fn sender_of(header: &Header<'_>) -> String {
    header.sender().map(|s| s.to_string()).unwrap_or_default()
}

#[interface(name = "org.Gtk.MountOperationHandler")]
impl MountHandler {
    #[allow(clippy::too_many_arguments)]
    async fn ask_password(
        &self,
        #[zbus(header)] header: Header<'_>,
        id: String,
        message: String,
        icon_name: String,
        default_user: String,
        default_domain: String,
        flags: u32,
    ) -> (u32, Details) {
        tracing::info!(id, flags, "mount operation asks for a password");
        let Some(mut prompt) = password_prompt(&message, &default_user, &default_domain, flags) else {
            return (result::UNHANDLED, Details::new());
        };
        prompt.icon = icon(&icon_name);
        // GTK reads no anonymous-login answer, so only VeraCrypt's are offered.
        let extra_flags = flags & flags::TCRYPT;
        let has_user = prompt.entry.is_some();
        add_extras(&mut prompt, extra_flags);
        self.run(sender_of(&header), id, prompt, |answer| match answer {
            Answer::Continue { entry, password, choice, checks, .. } => {
                if flags & flags::SAVING_SUPPORTED != 0 {
                    remember_chosen(choice);
                }
                let password = password.map(|p| String::from_utf8_lossy(p.bytes()).into_owned());
                let named = if has_user { entry.as_deref() } else { None };
                let mut details = password_details(flags, &default_user, &default_domain, named, password.as_deref(), choice);
                tcrypt_details(&mut details, extra_flags, &extras_of(extra_flags, has_user, entry.as_deref(), &checks));
                (result::HANDLED, details)
            }
            Answer::Cancel => (result::ABORTED, Details::new()),
        })
        .await
    }

    async fn ask_question(&self, #[zbus(header)] header: Header<'_>, id: String, message: String, icon_name: String, choices: Vec<String>) -> (u32, Details) {
        tracing::info!(id, choices = choices.len(), "mount operation asks a question");
        let (title, rest) = split_message(&message);
        let prompt = Prompt { title, message: rest, icon: icon(&icon_name), ..Prompt::default() };
        self.question(sender_of(&header), id, prompt, &choices).await
    }

    async fn show_processes(
        &self,
        #[zbus(header)] header: Header<'_>,
        id: String,
        message: String,
        icon_name: String,
        application_pids: Vec<i32>,
        choices: Vec<String>,
    ) -> (u32, Details) {
        tracing::info!(id, pids = ?application_pids, "mount operation shows busy apps");
        let (title, rest) = split_message(&message);
        let names: Vec<String> = application_pids.iter().map(|p| crate::media::process_name(*p)).collect();
        let description = if names.is_empty() { String::new() } else { format!("In use by: {}", crate::media::program_list(&names)) };
        let prompt = Prompt { title, message: rest, description, icon: icon(&icon_name), ..Prompt::default() };
        self.question(sender_of(&header), id, prompt, &choices).await
    }

    /// Dismiss the caller's dialog (its call answers 2).
    async fn close(&self, #[zbus(header)] header: Header<'_>) {
        let sender = sender_of(&header);
        let closing = {
            let mut current = self.current();
            match current.as_ref() {
                Some(c) if c.sender == sender => current.take(),
                _ => None,
            }
        };
        if let Some(c) = closing {
            tracing::info!(id = c.id, "mount operation dialog closed by its app");
            let _ = c.stop.send(Stop::Closed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(details: &Details, key: &str) -> Option<String> {
        details.get(key).and_then(|v| String::try_from(v.clone()).ok())
    }

    #[test]
    fn extras() {
        let mut p = password_prompt("Unlock", "", "", flags::NEED_PASSWORD | flags::TCRYPT).unwrap();
        add_extras(&mut p, flags::NEED_PASSWORD | flags::TCRYPT);
        assert_eq!(p.entry.as_ref().map(|e| e.0.as_str()), Some("PIM (optional)"));
        assert_eq!(p.checks.len(), 2);
        let e = extras_of(flags::TCRYPT, false, Some(" 485 "), &[true, false]);
        assert_eq!(e, Extras { anonymous: false, hidden_volume: true, system_volume: false, pim: Some(485) });
        let a = extras_of(flags::ANONYMOUS_SUPPORTED, true, Some("bob"), &[true]);
        assert!(a.anonymous && a.pim.is_none());
        let mut d = Details::new();
        tcrypt_details(&mut d, flags::TCRYPT, &e);
        assert_eq!(bool::try_from(d["hidden_volume"].clone()), Ok(true));
        assert_eq!(bool::try_from(d["system_volume"].clone()), Ok(false));
        assert_eq!(u32::try_from(d["pim"].clone()), Ok(485));
        let mut none = Details::new();
        tcrypt_details(&mut none, flags::NEED_PASSWORD, &e);
        assert!(none.is_empty());
    }

    #[test]
    fn password_dialogs() {
        assert!(password_prompt("x", "", "", flags::SAVING_SUPPORTED).is_none());
        let p = password_prompt("Enter password for share\non server", "bob", "WORK", flags::NEED_PASSWORD | flags::NEED_USERNAME | flags::NEED_DOMAIN | flags::SAVING_SUPPORTED).unwrap();
        assert_eq!((p.title.as_str(), p.message.as_str()), ("Enter password for share", "on server"));
        assert_eq!(p.entry, Some(("User name (domain\\name)".into(), "WORK\\bob".into())));
        assert!(p.password.is_some() && p.choice.is_some());
        let d = password_details(flags::NEED_PASSWORD | flags::NEED_USERNAME | flags::NEED_DOMAIN | flags::SAVING_SUPPORTED, "bob", "WORK", Some("HOME\\ann"), Some("pw"), true);
        assert_eq!((s(&d, "username"), s(&d, "domain"), s(&d, "password")), (Some("ann".into()), Some("HOME".into()), Some("pw".into())));
        assert_eq!(d.get("password_save").and_then(|v| u32::try_from(v.clone()).ok()), Some(password_save::PERMANENTLY));
        let d = password_details(flags::NEED_USERNAME | flags::NEED_DOMAIN, "bob", "WORK", Some("ann"), None, true);
        assert_eq!((s(&d, "username"), s(&d, "domain"), s(&d, "password")), (Some("ann".into()), Some("WORK".into()), None));
        assert_eq!(d.get("password_save").and_then(|v| u32::try_from(v.clone()).ok()), Some(password_save::NEVER));
        let p = password_prompt("Password", "", "", flags::NEED_PASSWORD).unwrap();
        assert_eq!((p.entry, p.choice), (None, None));
    }

    fn cont(selected: usize) -> Answer {
        Answer::Continue { entry: None, password: None, confirm: None, choice: false, checks: Vec::new(), selected, picked: Vec::new(), place: None }
    }

    #[test]
    fn choices() {
        let two = ["Log In Anyway".to_owned(), "Cancel Login".to_owned()];
        let (p, map) = choice_prompt(Prompt::default(), &two).unwrap();
        assert_eq!((p.continue_label.as_str(), p.cancel_label.as_str()), ("Log In Anyway", "Cancel Login"));
        assert_eq!(map(Answer::Cancel), Some(1));
        let (_, map) = choice_prompt(Prompt::default(), &two[..1]).unwrap();
        assert_eq!(map(Answer::Cancel), None);
        let three = ["Cancel".to_owned(), "Force".to_owned(), "Wait".to_owned()];
        let (p, map) = choice_prompt(Prompt::default(), &three).unwrap();
        assert_eq!(p.list, three);
        assert_eq!(map(cont(2)), Some(2));
        let (_, map) = choice_prompt(Prompt::default(), &three).unwrap();
        assert_eq!(map(Answer::Cancel), None);
        assert!(choice_prompt(Prompt::default(), &[]).is_none());
        assert_eq!(split_message("One"), ("One".into(), String::new()));
    }
}
