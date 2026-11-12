//! NetworkManager's secret agent (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.NetworkManager.SecretAgent.md`): asks
//! the user for Wi-Fi passwords.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{interface, Connection};

use crate::prompt::{self, Answer, Prompt};
use crate::UiEvent;

pub const AGENT_PATH: &str = "/org/freedesktop/NetworkManager/SecretAgent";
const NM: &str = "org.freedesktop.NetworkManager";
const AGENT_MANAGER_PATH: &str = "/org/freedesktop/NetworkManager/AgentManager";
const AGENT_MANAGER: &str = "org.freedesktop.NetworkManager.AgentManager";
pub const IDENTIFIER: &str = "org.gnome.Shell.NetworkAgent";
const SECURITY: &str = "802-11-wireless-security";

const ALLOW_INTERACTION: u32 = 0x1;
const REQUEST_NEW: u32 = 0x2;

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.NetworkManager.SecretAgent")]
pub enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    UserCanceled(String),
    NoSecrets(String),
    AgentCanceled(String),
}

/// Which secret a Wi-Fi network needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wanted {
    Psk,
    Wep,
}

impl Wanted {
    pub fn key(self) -> &'static str {
        match self {
            Wanted::Psk => "psk",
            Wanted::Wep => "wep-key0",
        }
    }

    /// Whether `password` can be this kind of secret.
    pub fn valid(self, password: &str) -> bool {
        let hex = |n: usize| password.len() == n && password.chars().all(|c| c.is_ascii_hexdigit());
        let chars = password.chars().count();
        match self {
            Wanted::Psk => (8..=63).contains(&chars) || hex(64),
            Wanted::Wep => chars == 5 || chars == 13 || hex(10) || hex(26),
        }
    }
}

type Settings = HashMap<String, HashMap<String, OwnedValue>>;

fn text(settings: &Settings, group: &str, key: &str) -> Option<String> {
    settings.get(group)?.get(key).and_then(|v| String::try_from(v.clone()).ok())
}

/// What to ask for: the setting's secret, and the network's name.
pub fn wanted(settings: &Settings, setting_name: &str) -> Option<(Wanted, String)> {
    if setting_name != SECURITY {
        return None;
    }
    let wanted = match text(settings, SECURITY, "key-mgmt")?.as_str() {
        "wpa-psk" | "sae" => Wanted::Psk,
        "none" => Wanted::Wep,
        _ => return None,
    };
    let ssid = settings
        .get("802-11-wireless")
        .and_then(|w| w.get("ssid"))
        .and_then(|v| Vec::<u8>::try_from(v.clone()).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .or_else(|| text(settings, "connection", "id"))
        .unwrap_or_default();
    Some((wanted, ssid))
}

/// Open dialogs by (connection path, setting), to cancel them.
type Pending = Arc<Mutex<HashMap<(String, String), oneshot::Sender<()>>>>;

pub struct Agent {
    ui: mpsc::UnboundedSender<UiEvent>,
    pending: Pending,
}

#[interface(name = "org.freedesktop.NetworkManager.SecretAgent")]
impl Agent {
    async fn get_secrets(
        &self,
        connection: Settings,
        connection_path: OwnedObjectPath,
        setting_name: String,
        _hints: Vec<String>,
        flags: u32,
    ) -> Result<HashMap<String, HashMap<String, OwnedValue>>, AgentError> {
        let Some((wanted, ssid)) = wanted(&connection, &setting_name) else {
            return Err(AgentError::NoSecrets(format!("Helix has no secrets for {setting_name}")));
        };
        if flags & ALLOW_INTERACTION == 0 {
            return Err(AgentError::NoSecrets("no interaction allowed".into()));
        }
        tracing::info!(ssid, ?wanted, flags, "Wi-Fi password asked");
        let key = (connection_path.to_string(), setting_name.clone());
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), cancel_tx);
        let _turn = prompt::turn().await;
        let mut warning = if flags & REQUEST_NEW != 0 { "Wrong password? Try again.".to_owned() } else { String::new() };
        let result = loop {
            let ask = prompt::ask(
                &self.ui,
                Prompt {
                    title: "Wi-Fi Network".into(),
                    message: format!("A password is needed to connect to “{ssid}”."),
                    password: Some("Password".into()),
                    warning: warning.clone(),
                    continue_label: "Connect".into(),
                    cancel_label: "Cancel".into(),
                    ..Prompt::default()
                },
            );
            let answer = tokio::select! {
                a = ask => a,
                _ = &mut cancel_rx => break Err(AgentError::AgentCanceled("canceled by NetworkManager".into())),
            };
            match answer {
                Answer::Continue { password: Some(p), .. } => {
                    let password = String::from_utf8_lossy(p.bytes()).into_owned();
                    if wanted.valid(&password) {
                        let secret = HashMap::from([(wanted.key().to_owned(), OwnedValue::try_from(Value::from(password)).map_err(|e| AgentError::ZBus(e.into()))?)]);
                        break Ok(HashMap::from([(SECURITY.to_owned(), secret)]));
                    }
                    warning = "This password is not valid.".into();
                }
                _ => break Err(AgentError::UserCanceled("canceled".into())),
            }
        };
        prompt::done(&self.ui);
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        tracing::info!(ssid, answered = result.is_ok(), "Wi-Fi password");
        result
    }

    async fn cancel_get_secrets(&self, connection_path: OwnedObjectPath, setting_name: String) {
        if let Some(cancel) = self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&(connection_path.to_string(), setting_name)) {
            let _ = cancel.send(());
        }
    }

    async fn save_secrets(&self, _connection: Settings, _connection_path: OwnedObjectPath) {}

    async fn delete_secrets(&self, _connection: Settings, _connection_path: OwnedObjectPath) {}
}

async fn register(system: &Connection) {
    match system.call_method(Some(NM), AGENT_MANAGER_PATH, Some(AGENT_MANAGER), "RegisterWithCapabilities", &(IDENTIFIER, 0u32)).await {
        Ok(_) => tracing::info!("NetworkManager secret agent registered"),
        Err(err) => tracing::info!(%err, "NetworkManager did not take the secret agent"),
    }
}

/// Serve the agent on `system` for the rest of the session.
pub async fn run(system: Connection, ui: mpsc::UnboundedSender<UiEvent>) -> zbus::Result<()> {
    system.object_server().at(AGENT_PATH, Agent { ui, pending: Default::default() }).await?;
    let dbus = zbus::fdo::DBusProxy::new(&system).await?;
    let mut owners = dbus.receive_name_owner_changed_with_args(&[(0, NM)]).await?;
    if dbus.name_has_owner(zbus::names::BusName::from_static_str(NM)?).await.unwrap_or(false) {
        register(&system).await;
    }
    while let Some(change) = owners.next().await {
        if change.args().is_ok_and(|a| a.new_owner().is_some()) {
            register(&system).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(key_mgmt: &str) -> Settings {
        let v = |x: Value<'_>| OwnedValue::try_from(x).unwrap();
        HashMap::from([
            ("802-11-wireless".to_owned(), HashMap::from([("ssid".to_owned(), v(Value::from(b"HomeNet".to_vec())))])),
            (SECURITY.to_owned(), HashMap::from([("key-mgmt".to_owned(), v(Value::from(key_mgmt)))])),
        ])
    }

    #[test]
    fn requests() {
        assert_eq!(wanted(&settings("wpa-psk"), SECURITY), Some((Wanted::Psk, "HomeNet".into())));
        assert_eq!(wanted(&settings("sae"), SECURITY).map(|w| w.0), Some(Wanted::Psk));
        assert_eq!(wanted(&settings("none"), SECURITY).map(|w| w.0), Some(Wanted::Wep));
        assert_eq!(wanted(&settings("wpa-eap"), SECURITY), None);
        assert_eq!(wanted(&settings("wpa-psk"), "802-1x"), None);
    }

    #[test]
    fn password_rules() {
        assert!(!Wanted::Psk.valid("short"));
        assert!(Wanted::Psk.valid("long enough"));
        assert!(Wanted::Psk.valid(&"a".repeat(64)), "64 hex digits");
        assert!(!Wanted::Psk.valid(&"g".repeat(64)));
        assert!(Wanted::Wep.valid("abcde") && Wanted::Wep.valid("0123456789"));
        assert!(!Wanted::Wep.valid("abcdef"));
    }
}
