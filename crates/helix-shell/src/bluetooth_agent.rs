//! BlueZ's pairing agent (helix-desktop-spec
//! `contracts/dbus/org.bluez.Agent1.md`): shows and asks for pairing codes,
//! and authorizes devices.

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::{interface, Connection};

use crate::notifications::Center;
use crate::prompt::{self, Answer, Prompt};
use crate::UiEvent;

pub const AGENT_PATH: &str = "/org/helix/BluetoothAgent";
const BLUEZ: &str = "org.bluez";
const CAPABILITY: &str = "KeyboardDisplay";

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
pub enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
    Canceled(String),
}

/// A passkey as BlueZ shows it: six digits.
pub fn passkey_text(passkey: u32) -> String {
    format!("{passkey:06}")
}

/// A typed PIN (1 to 16 characters) or passkey (0 to 999999).
pub fn valid_pin(pin: &str) -> bool {
    (1..=16).contains(&pin.chars().count())
}

pub fn parse_passkey(text: &str) -> Option<u32> {
    text.trim().parse::<u32>().ok().filter(|n| *n <= 999_999)
}

pub struct Agent {
    system: Connection,
    ui: mpsc::UnboundedSender<UiEvent>,
    center: Center,
    /// The open dialog's cancel switch.
    cancel: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    /// The code notification shown.
    notice: Arc<Mutex<Option<u32>>>,
}

enum Asked {
    Answer(Answer),
    Canceled,
}

impl Agent {
    async fn name(&self, device: &ObjectPath<'_>) -> String {
        let proxy = zbus::Proxy::new(&self.system, BLUEZ, device.to_owned(), "org.bluez.Device1").await;
        match proxy {
            Ok(p) => match p.get_property::<String>("Alias").await {
                Ok(alias) => alias,
                Err(_) => p.get_property::<String>("Address").await.unwrap_or_else(|_| "A device".into()),
            },
            Err(_) => "A device".into(),
        }
    }

    /// Show `prompt`; `Cancel()` from BlueZ closes it.
    async fn ask(&self, prompt: Prompt) -> Asked {
        let (tx, rx) = oneshot::channel();
        *self.cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        let _turn = prompt::turn().await;
        let asked = tokio::select! {
            answer = prompt::ask(&self.ui, prompt) => Asked::Answer(answer),
            _ = rx => Asked::Canceled,
        };
        prompt::done(&self.ui);
        self.cancel.lock().unwrap_or_else(|e| e.into_inner()).take();
        asked
    }

    async fn yes_no(&self, title: String, message: String, yes: &str, no: &str) -> Result<(), AgentError> {
        match self.ask(Prompt { title, message, continue_label: yes.into(), cancel_label: no.into(), ..Prompt::default() }).await {
            Asked::Answer(Answer::Continue { .. }) => Ok(()),
            Asked::Answer(Answer::Cancel) => Err(AgentError::Rejected("refused".into())),
            Asked::Canceled => Err(AgentError::Canceled("canceled".into())),
        }
    }

    /// Ask for a code until it is valid by `check`.
    async fn code(&self, name: &str, field: &str, check: impl Fn(&str) -> bool) -> Result<String, AgentError> {
        let mut warning = String::new();
        loop {
            let prompt = Prompt {
                title: format!("Pair with {name}"),
                message: format!("Type the code {name} asks for."),
                password: Some(field.into()),
                warning: warning.clone(),
                continue_label: "Pair".into(),
                cancel_label: "Cancel".into(),
                ..Prompt::default()
            };
            match self.ask(prompt).await {
                Asked::Answer(Answer::Continue { password: Some(p), .. }) => {
                    let text = String::from_utf8_lossy(p.bytes()).into_owned();
                    if check(&text) {
                        return Ok(text);
                    }
                    warning = "This code is not valid.".into();
                }
                Asked::Answer(_) => return Err(AgentError::Rejected("refused".into())),
                Asked::Canceled => return Err(AgentError::Canceled("canceled".into())),
            }
        }
    }

    fn show_code(&self, name: &str, code: &str) {
        let mut notice = self.notice.lock().unwrap_or_else(|e| e.into_inner());
        if notice.is_none() {
            *notice = Some(self.center.notice("Bluetooth", &format!("Pair with {name}"), &format!("Type {code} on {name}, then press Enter."), "bluetooth-active-symbolic", false));
        }
    }

    fn withdraw_code(&self) {
        if let Some(id) = self.notice.lock().unwrap_or_else(|e| e.into_inner()).take() {
            self.center.withdraw(id);
        }
    }
}

#[interface(name = "org.bluez.Agent1")]
impl Agent {
    async fn release(&self) {
        tracing::info!("BlueZ released the pairing agent");
    }

    async fn request_pin_code(&self, device: ObjectPath<'_>) -> Result<String, AgentError> {
        let name = self.name(&device).await;
        self.code(&name, "PIN", valid_pin).await
    }

    async fn display_pin_code(&self, device: ObjectPath<'_>, pincode: String) {
        let name = self.name(&device).await;
        self.show_code(&name, &pincode);
    }

    async fn request_passkey(&self, device: ObjectPath<'_>) -> Result<u32, AgentError> {
        let name = self.name(&device).await;
        let text = self.code(&name, "Passkey", |t| parse_passkey(t).is_some()).await?;
        parse_passkey(&text).ok_or_else(|| AgentError::Rejected("not a passkey".into()))
    }

    async fn display_passkey(&self, device: ObjectPath<'_>, passkey: u32, _entered: u16) {
        let name = self.name(&device).await;
        self.show_code(&name, &passkey_text(passkey));
    }

    async fn request_confirmation(&self, device: ObjectPath<'_>, passkey: u32) -> Result<(), AgentError> {
        let name = self.name(&device).await;
        tracing::info!(name, "Bluetooth pairing confirmation");
        self.yes_no(format!("Pair with {name}?"), format!("Confirm that {} is shown on {name}.", passkey_text(passkey)), "Pair", "Cancel").await
    }

    async fn request_authorization(&self, device: ObjectPath<'_>) -> Result<(), AgentError> {
        let name = self.name(&device).await;
        self.yes_no(name.clone(), format!("{name} wants to pair with this computer."), "Allow", "Deny").await
    }

    async fn authorize_service(&self, device: ObjectPath<'_>, _uuid: String) -> Result<(), AgentError> {
        let name = self.name(&device).await;
        self.yes_no(name.clone(), format!("{name} wants to connect to this computer."), "Allow", "Deny").await
    }

    async fn cancel(&self) {
        if let Some(cancel) = self.cancel.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = cancel.send(());
        }
        self.withdraw_code();
    }
}

async fn register(system: &Connection) {
    let path = OwnedObjectPath::try_from(AGENT_PATH).expect("a valid path");
    let manager = "org.bluez.AgentManager1";
    let registered = async {
        system.call_method(Some(BLUEZ), "/org/bluez", Some(manager), "RegisterAgent", &(&path, CAPABILITY)).await?;
        system.call_method(Some(BLUEZ), "/org/bluez", Some(manager), "RequestDefaultAgent", &(&path,)).await
    };
    match registered.await {
        Ok(_) => tracing::info!("Bluetooth pairing agent registered"),
        Err(err) => tracing::info!(%err, "BlueZ did not take the pairing agent"),
    }
}

/// Serve the agent on `system` for the rest of the session.
pub async fn run(system: Connection, ui: mpsc::UnboundedSender<UiEvent>, center: Center) -> zbus::Result<()> {
    let notice: Arc<Mutex<Option<u32>>> = Default::default();
    let _ = NOTICE.set((center.clone(), notice.clone()));
    let agent = Agent { system: system.clone(), ui, center, cancel: Default::default(), notice };
    system.object_server().at(AGENT_PATH, agent).await?;
    let dbus = zbus::fdo::DBusProxy::new(&system).await?;
    let mut owners = dbus.receive_name_owner_changed_with_args(&[(0, BLUEZ)]).await?;
    if dbus.name_has_owner(zbus::names::BusName::from_static_str(BLUEZ)?).await.unwrap_or(false) {
        register(&system).await;
    }
    while let Some(change) = owners.next().await {
        if change.args().is_ok_and(|a| a.new_owner().is_some()) {
            register(&system).await;
        }
    }
    Ok(())
}

/// A pairing ended (paired or failed): withdraw its code notification.
pub fn pairing_done() {
    if let Some((center, notice)) = NOTICE.get() {
        if let Some(id) = notice.lock().unwrap_or_else(|e| e.into_inner()).take() {
            center.withdraw(id);
        }
    }
}

/// The code notification, reachable from outside the agent.
static NOTICE: std::sync::OnceLock<(Center, Arc<Mutex<Option<u32>>>)> = std::sync::OnceLock::new();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        assert_eq!(passkey_text(42), "000042");
        assert_eq!(parse_passkey(" 123456 "), Some(123_456));
        assert_eq!(parse_passkey("1000000"), None);
        assert_eq!(parse_passkey("12a"), None);
        assert!(valid_pin("0000") && !valid_pin("") && !valid_pin(&"1".repeat(17)));
    }
}
