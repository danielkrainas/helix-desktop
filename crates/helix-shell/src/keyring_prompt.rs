//! gcr's system prompter (helix-desktop-spec
//! `org.gnome.keyring.internal.Prompter.md`): keyring and other gcr
//! password prompts, in the shell's prompt dialog.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::message::Header;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{interface, Connection};

use crate::prompt::{self, Answer, Prompt};
use crate::secret_exchange::Exchange;
use crate::UiEvent;

pub const BUS_NAME: &str = "org.gnome.keyring.SystemPrompter";
pub const OBJECT_PATH: &str = "/org/gnome/keyring/Prompter";
const CALLBACK: &str = "org.gnome.keyring.internal.Prompter.Callback";
const MISMATCH: &str = "The passwords do not match.";

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.gnome.keyring.Prompter")]
pub enum PrompterError {
    #[zbus(error)]
    ZBus(zbus::Error),
    InProgress(String),
    Failed(String),
}

/// A client: its unique bus name and callback object.
type Client = (String, OwnedObjectPath);

/// The prompt properties so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Props {
    pub title: String,
    pub message: String,
    pub description: String,
    pub warning: String,
    pub password_new: bool,
    pub choice_label: String,
    pub choice_chosen: bool,
    pub continue_label: String,
    pub cancel_label: String,
}

impl Props {
    pub fn update(&mut self, changed: &HashMap<String, OwnedValue>) {
        for (key, value) in changed {
            let text = || match &**value {
                Value::Str(s) => s.to_string(),
                _ => String::new(),
            };
            let flag = || matches!(&**value, Value::Bool(true));
            match key.as_str() {
                "title" => self.title = text(),
                "message" => self.message = text(),
                "description" => self.description = text(),
                "warning" => self.warning = text(),
                "password-new" => self.password_new = flag(),
                "choice-label" => self.choice_label = text(),
                "choice-chosen" => self.choice_chosen = flag(),
                "continue-label" => self.continue_label = text(),
                "cancel-label" => self.cancel_label = text(),
                _ => {}
            }
        }
    }

    /// The dialog for a prompt of `kind` (`password` or `confirm`).
    pub fn prompt(&self, password: bool) -> Prompt {
        let or = |s: &str, d: &str| if s.is_empty() { d.to_owned() } else { s.to_owned() };
        Prompt {
            title: or(&self.title, "Unlock"),
            message: self.message.clone(),
            description: self.description.clone(),
            password: password.then(|| "Password".to_owned()),
            confirm: (password && self.password_new).then(|| "Confirm".to_owned()),
            choice: (!self.choice_label.is_empty()).then(|| (self.choice_label.clone(), self.choice_chosen)),
            warning: self.warning.clone(),
            continue_label: or(&self.continue_label, "Continue"),
            cancel_label: or(&self.cancel_label, "Cancel"),
            ..Prompt::default()
        }
    }
}

struct Active {
    client: Client,
    /// Tells this activation from a later one of the same client.
    generation: u64,
    exchange: Exchange,
    props: Props,
    _turn: tokio::sync::OwnedMutexGuard<()>,
}

#[derive(Default)]
struct State {
    waiting: VecDeque<Client>,
    active: Option<Active>,
    starting: bool,
    generation: u64,
}

type Shared = Arc<Mutex<State>>;

fn lock(state: &Shared) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

async fn call_ready(conn: &Connection, client: &Client, reply: &str, properties: HashMap<&str, Value<'_>>, exchange: &str) -> zbus::Result<()> {
    conn.call_method(Some(client.0.as_str()), client.1.as_str(), Some(CALLBACK), "PromptReady", &(reply, properties, exchange)).await.map(drop)
}

async fn call_done(conn: &Connection, client: &Client) {
    let _ = conn.call_method(Some(client.0.as_str()), client.1.as_str(), Some(CALLBACK), "PromptDone", &()).await;
}

/// Start the next waiting client, if none is active.
fn advance(state: Shared, conn: Connection) {
    {
        let mut s = lock(&state);
        if s.active.is_some() || s.starting || s.waiting.is_empty() {
            return;
        }
        s.starting = true;
    }
    tokio::spawn(async move {
        let turn = prompt::turn().await;
        let (client, begin) = {
            let mut s = lock(&state);
            s.starting = false;
            let Some(client) = s.waiting.pop_front() else { return };
            let exchange = Exchange::new();
            let begin = exchange.begin();
            s.generation += 1;
            let generation = s.generation;
            s.active = Some(Active { client: client.clone(), generation, exchange, props: Props::default(), _turn: turn });
            (client, begin)
        };
        if let Err(err) = call_ready(&conn, &client, "", HashMap::new(), &begin).await {
            tracing::info!(%err, client = client.0, "keyring prompt client is gone");
            lock(&state).active = None;
            advance(state, conn);
        }
    });
}

/// End `client`'s prompting (stopped, or gone from the bus).
fn finish(state: &Shared, ui: &mpsc::UnboundedSender<UiEvent>, client: &Client) -> bool {
    let mut s = lock(state);
    if s.active.as_ref().is_some_and(|a| &a.client == client) {
        s.active = None;
        drop(s);
        prompt::done(ui);
        return true;
    }
    let before = s.waiting.len();
    s.waiting.retain(|c| c != client);
    s.waiting.len() != before
}

/// The served prompter (clones share it).
#[derive(Clone)]
pub struct Prompter {
    state: Shared,
    ui: mpsc::UnboundedSender<UiEvent>,
}

impl Prompter {
    pub fn new(ui: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self { state: Default::default(), ui }
    }

    /// Close the prompts of clients that leave the bus.
    pub fn follow_clients(&self, conn: Connection) {
        let (state, ui) = (self.state.clone(), self.ui.clone());
        tokio::spawn(async move {
            let Ok(dbus) = zbus::fdo::DBusProxy::new(&conn).await else { return };
            let Ok(mut changes) = dbus.receive_name_owner_changed().await else { return };
            while let Some(change) = changes.next().await {
                let Ok(args) = change.args() else { continue };
                let name = args.name().to_string();
                if !name.starts_with(':') || args.new_owner().is_some() {
                    continue;
                }
                let clients: Vec<Client> = {
                    let s = lock(&state);
                    s.active.iter().map(|a| a.client.clone()).chain(s.waiting.iter().cloned()).filter(|c| c.0 == name).collect()
                };
                for client in clients {
                    tracing::info!(client = client.0, "keyring prompt client left");
                    finish(&state, &ui, &client);
                }
                advance(state.clone(), conn.clone());
            }
        });
    }
}

fn client_of(header: &Header<'_>, callback: OwnedObjectPath) -> Client {
    (header.sender().map(|s| s.to_string()).unwrap_or_default(), callback)
}

#[interface(name = "org.gnome.keyring.internal.Prompter")]
impl Prompter {
    async fn begin_prompting(&self, callback: OwnedObjectPath, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) {
        let client = client_of(&header, callback);
        tracing::debug!(client = client.0, "keyring prompt requested");
        lock(&self.state).waiting.push_back(client);
        advance(self.state.clone(), conn.clone());
    }

    async fn perform_prompt(
        &self,
        callback: OwnedObjectPath,
        kind: &str,
        properties: HashMap<String, OwnedValue>,
        exchange: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<(), PrompterError> {
        let client = client_of(&header, callback);
        let password = match kind {
            "password" => true,
            "confirm" => false,
            other => return Err(PrompterError::Failed(format!("unknown prompt type {other}"))),
        };
        let (props, generation) = {
            let mut s = lock(&self.state);
            let active = s.active.as_mut().filter(|a| a.client == client).ok_or_else(|| PrompterError::InProgress("not this client's turn".into()))?;
            active.props.update(&properties);
            active.exchange.receive(exchange).map_err(PrompterError::Failed)?;
            (active.props.clone(), active.generation)
        };
        let (state, ui, conn) = (self.state.clone(), self.ui.clone(), conn.clone());
        tokio::spawn(async move {
            let mut dialog = props.prompt(password);
            let answer = loop {
                match prompt::ask(&ui, dialog.clone()).await {
                    Answer::Continue { password: Some(p), confirm: Some(c), choice, .. } if p.bytes() != c.bytes() => {
                        dialog.warning = MISMATCH.into();
                        dialog.choice = dialog.choice.map(|(l, _)| (l, choice));
                    }
                    other => break other,
                }
            };
            let (reply, secret, choice) = match answer {
                Answer::Continue { password, choice, .. } => ("yes", password, choice),
                Answer::Cancel => ("no", None, props.choice_chosen),
            };
            // Still this client's prompt?
            let sent = {
                let mut s = lock(&state);
                let Some(active) = s.active.as_mut().filter(|a| a.generation == generation) else { return };
                active.props.choice_chosen = choice;
                let secret = if reply == "yes" && password { secret.as_ref().map(|s| s.bytes()) } else { None };
                active.exchange.send(secret)
            };
            let Ok(sent) = sent else { return };
            // Values go back wrapped in a second variant, as gcr reads them.
            let boxed = |v: Value<'static>| Value::Value(Box::new(v));
            let mut changed: HashMap<&str, Value<'_>> = HashMap::from([("choice-chosen", boxed(Value::from(choice)))]);
            if password {
                let strength = i32::from(secret.as_ref().is_some_and(|s| !s.bytes().is_empty()));
                changed.insert("password-strength", boxed(Value::from(strength)));
            }
            if let Err(err) = call_ready(&conn, &client, reply, changed, &sent).await {
                tracing::info!(%err, "keyring prompt client did not take the answer");
            }
        });
        Ok(())
    }

    async fn stop_prompting(&self, callback: OwnedObjectPath, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) {
        let client = client_of(&header, callback);
        if finish(&self.state, &self.ui, &client) {
            call_done(conn, &client).await;
        }
        advance(self.state.clone(), conn.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn properties_to_dialog() {
        let mut props = Props::default();
        let owned = |v: Value<'_>| OwnedValue::try_from(v).unwrap();
        props.update(&HashMap::from([
            ("title".to_owned(), owned(Value::from("Unlock Keyring"))),
            ("password-new".to_owned(), owned(Value::from(true))),
            ("choice-label".to_owned(), owned(Value::from("Remember"))),
            ("continue-label".to_owned(), owned(Value::from("Unlock"))),
        ]));
        let p = props.prompt(true);
        assert_eq!(p.title, "Unlock Keyring");
        assert_eq!(p.confirm.as_deref(), Some("Confirm"));
        assert_eq!(p.choice, Some(("Remember".into(), false)));
        assert_eq!((p.continue_label.as_str(), p.cancel_label.as_str()), ("Unlock", "Cancel"));
        let c = Props::default().prompt(false);
        assert_eq!((c.title.as_str(), c.password.is_none()), ("Unlock", true));
    }
}
