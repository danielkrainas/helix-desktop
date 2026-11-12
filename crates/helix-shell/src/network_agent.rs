//! NetworkManager's secret agent (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.NetworkManager.SecretAgent.md`): asks
//! the user for Wi-Fi passwords and 802.1X (enterprise) credentials.

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
const DOT1X: &str = "802-1x";

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

/// Which secret a network needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wanted {
    Psk,
    Wep,
    /// LEAP: a user name and password in the Wi-Fi security setting.
    Leap,
    /// 802.1X: a user name, and a password (or, with TLS, the private
    /// key's password).
    Eap { tls: bool },
}

impl Wanted {
    /// Whether `password` can be this kind of secret.
    pub fn valid(self, password: &str) -> bool {
        let hex = |n: usize| password.len() == n && password.chars().all(|c| c.is_ascii_hexdigit());
        let chars = password.chars().count();
        match self {
            Wanted::Psk => (8..=63).contains(&chars) || hex(64),
            Wanted::Wep => chars == 5 || chars == 13 || hex(10) || hex(26),
            Wanted::Leap | Wanted::Eap { .. } => chars > 0,
        }
    }

    /// Whether a user name is asked too.
    pub fn needs_user(self) -> bool {
        matches!(self, Wanted::Leap | Wanted::Eap { .. })
    }

    /// The secrets to hand back: setting name and its keys.
    pub fn secrets(self, user: &str, password: String) -> (&'static str, Vec<(&'static str, String)>) {
        match self {
            Wanted::Psk => (SECURITY, vec![("psk", password)]),
            Wanted::Wep => (SECURITY, vec![("wep-key0", password)]),
            Wanted::Leap => (SECURITY, vec![("leap-username", user.to_owned()), ("leap-password", password)]),
            Wanted::Eap { tls: false } => (DOT1X, vec![("identity", user.to_owned()), ("password", password)]),
            Wanted::Eap { tls: true } => (DOT1X, vec![("identity", user.to_owned()), ("private-key-password", password)]),
        }
    }
}

type Settings = HashMap<String, HashMap<String, OwnedValue>>;

fn text(settings: &Settings, group: &str, key: &str) -> Option<String> {
    settings.get(group)?.get(key).and_then(|v| String::try_from(v.clone()).ok())
}

/// What to ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    pub wanted: Wanted,
    /// The network's name.
    pub name: String,
    /// The user name the connection has.
    pub user: String,
    /// A wired (Ethernet) connection.
    pub wired: bool,
}

/// What to ask for, for `setting_name` of a connection.
pub fn wanted(settings: &Settings, setting_name: &str) -> Option<Need> {
    let (wanted, user) = match setting_name {
        SECURITY => match text(settings, SECURITY, "key-mgmt")?.as_str() {
            "wpa-psk" | "sae" => (Wanted::Psk, String::new()),
            "none" | "ieee8021x" if text(settings, SECURITY, "auth-alg").as_deref() == Some("leap") => (Wanted::Leap, text(settings, SECURITY, "leap-username").unwrap_or_default()),
            "none" => (Wanted::Wep, String::new()),
            _ => return None,
        },
        DOT1X => {
            let methods: Vec<String> = settings.get(DOT1X).and_then(|g| g.get("eap")).and_then(|v| Vec::<String>::try_from(v.clone()).ok()).unwrap_or_default();
            let tls = methods.first().is_some_and(|m| m == "tls");
            (Wanted::Eap { tls }, text(settings, DOT1X, "identity").unwrap_or_default())
        }
        _ => return None,
    };
    let wired = !settings.contains_key("802-11-wireless");
    let name = settings
        .get("802-11-wireless")
        .and_then(|w| w.get("ssid"))
        .and_then(|v| Vec::<u8>::try_from(v.clone()).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .or_else(|| text(settings, "connection", "id"))
        .unwrap_or_default();
    Some(Need { wanted, name, user, wired })
}

/// The dialog for `need`.
pub fn prompt_for(need: &Need, user: &str, warning: String) -> Prompt {
    let (message, password) = match need.wanted {
        Wanted::Psk | Wanted::Wep => (format!("A password is needed to connect to “{}”.", need.name), "Password"),
        Wanted::Leap | Wanted::Eap { tls: false } => (format!("A user name and password are needed to connect to “{}”.", need.name), "Password"),
        Wanted::Eap { tls: true } => (format!("Your identity and the password of your private key are needed to connect to “{}”.", need.name), "Private key password"),
    };
    Prompt {
        title: if need.wired { "Wired Network".into() } else { "Wi-Fi Network".into() },
        message,
        entry: need.wanted.needs_user().then(|| ("User name".to_owned(), user.to_owned())),
        password: Some(password.into()),
        warning,
        continue_label: "Connect".into(),
        cancel_label: "Cancel".into(),
        ..Prompt::default()
    }
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
        let Some(need) = wanted(&connection, &setting_name) else {
            return Err(AgentError::NoSecrets(format!("Helix has no secrets for {setting_name}")));
        };
        if flags & ALLOW_INTERACTION == 0 {
            return Err(AgentError::NoSecrets("no interaction allowed".into()));
        }
        let name = need.name.clone();
        tracing::info!(name, wanted = ?need.wanted, flags, "network secrets asked");
        let key = (connection_path.to_string(), setting_name.clone());
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), cancel_tx);
        let _turn = prompt::turn().await;
        let mut warning = if flags & REQUEST_NEW != 0 { "Wrong password? Try again.".to_owned() } else { String::new() };
        let mut user = need.user.clone();
        let result = loop {
            let ask = prompt::ask(&self.ui, prompt_for(&need, &user, warning.clone()));
            let answer = tokio::select! {
                a = ask => a,
                _ = &mut cancel_rx => break Err(AgentError::AgentCanceled("canceled by NetworkManager".into())),
            };
            match answer {
                Answer::Continue { password: Some(p), entry, .. } => {
                    let password = String::from_utf8_lossy(p.bytes()).into_owned();
                    user = entry.unwrap_or_default().trim().to_owned();
                    if need.wanted.needs_user() && user.is_empty() {
                        warning = "A user name is needed.".into();
                        continue;
                    }
                    if need.wanted.valid(&password) {
                        let (setting, keys) = need.wanted.secrets(&user, password);
                        let mut secret = HashMap::new();
                        for (k, v) in keys {
                            secret.insert(k.to_owned(), OwnedValue::try_from(Value::from(v)).map_err(|e| AgentError::ZBus(e.into()))?);
                        }
                        break Ok(HashMap::from([(setting.to_owned(), secret)]));
                    }
                    warning = "This password is not valid.".into();
                }
                _ => break Err(AgentError::UserCanceled("canceled".into())),
            }
        };
        prompt::done(&self.ui);
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        tracing::info!(name, answered = result.is_ok(), "network secrets");
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
        let need = |s: &Settings, name: &str| wanted(s, name).map(|n| n.wanted);
        let home = wanted(&settings("wpa-psk"), SECURITY).unwrap();
        assert_eq!((home.wanted, home.name.as_str(), home.wired), (Wanted::Psk, "HomeNet", false));
        assert_eq!(need(&settings("sae"), SECURITY), Some(Wanted::Psk));
        assert_eq!(need(&settings("none"), SECURITY), Some(Wanted::Wep));
        assert_eq!(need(&settings("wpa-eap"), SECURITY), None);
        assert_eq!(need(&settings("wpa-psk"), "802-1x"), Some(Wanted::Eap { tls: false }));
        let v = |x: Value<'_>| OwnedValue::try_from(x).unwrap();
        let mut leap = settings("ieee8021x");
        leap.get_mut(SECURITY).unwrap().extend([("auth-alg".to_owned(), v(Value::from("leap"))), ("leap-username".to_owned(), v(Value::from("ada")))]);
        let n = wanted(&leap, SECURITY).unwrap();
        assert_eq!((n.wanted, n.user.as_str()), (Wanted::Leap, "ada"));
        // Wired 802.1X with TLS.
        let wired: Settings = HashMap::from([
            ("connection".to_owned(), HashMap::from([("id".to_owned(), v(Value::from("Office LAN")))])),
            (DOT1X.to_owned(), HashMap::from([("eap".to_owned(), v(Value::from(vec!["tls"]))), ("identity".to_owned(), v(Value::from("ada@example.org")))])),
        ]);
        let n = wanted(&wired, DOT1X).unwrap();
        assert_eq!(n, Need { wanted: Wanted::Eap { tls: true }, name: "Office LAN".into(), user: "ada@example.org".into(), wired: true });
        let p = prompt_for(&n, &n.user, String::new());
        assert_eq!((p.title.as_str(), p.entry, p.password.as_deref()), ("Wired Network", Some(("User name".into(), "ada@example.org".into())), Some("Private key password")));
        assert_eq!(Wanted::Eap { tls: false }.secrets("ada", "pw".into()), (DOT1X, vec![("identity", "ada".into()), ("password", "pw".into())]));
        assert_eq!(Wanted::Leap.secrets("ada", "pw".into()).0, SECURITY);
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
