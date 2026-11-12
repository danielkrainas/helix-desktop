//! NetworkManager's secret agent (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.NetworkManager.SecretAgent.md`): asks
//! the user for Wi-Fi passwords, 802.1X (enterprise) credentials, mobile
//! broadband and DSL passwords and VPN secrets (through the VPN plugin's
//! auth dialog, see [`crate::vpn_secrets`]).

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
const GSM: &str = "gsm";
const CDMA: &str = "cdma";
const PPPOE: &str = "pppoe";
const VPN: &str = "vpn";
const WIREGUARD: &str = "wireguard";

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
    /// A mobile broadband password (`gsm` or `cdma` setting).
    Mobile { cdma: bool },
    /// A SIM card's PIN.
    Pin,
    /// DSL (PPPoE): a user name and password.
    Pppoe,
}

impl Wanted {
    /// Whether `password` can be this kind of secret.
    pub fn valid(self, password: &str) -> bool {
        let hex = |n: usize| password.len() == n && password.chars().all(|c| c.is_ascii_hexdigit());
        let chars = password.chars().count();
        match self {
            Wanted::Psk => (8..=63).contains(&chars) || hex(64),
            Wanted::Wep => chars == 5 || chars == 13 || hex(10) || hex(26),
            Wanted::Leap | Wanted::Eap { .. } | Wanted::Mobile { .. } | Wanted::Pppoe => chars > 0,
            Wanted::Pin => (4..=8).contains(&password.len()) && password.chars().all(|c| c.is_ascii_digit()),
        }
    }

    /// Whether a user name is asked too.
    pub fn needs_user(self) -> bool {
        matches!(self, Wanted::Leap | Wanted::Eap { .. } | Wanted::Pppoe)
    }

    /// The secrets to hand back: setting name and its keys.
    pub fn secrets(self, user: &str, password: String) -> (&'static str, Vec<(&'static str, String)>) {
        match self {
            Wanted::Psk => (SECURITY, vec![("psk", password)]),
            Wanted::Wep => (SECURITY, vec![("wep-key0", password)]),
            Wanted::Leap => (SECURITY, vec![("leap-username", user.to_owned()), ("leap-password", password)]),
            Wanted::Eap { tls: false } => (DOT1X, vec![("identity", user.to_owned()), ("password", password)]),
            Wanted::Eap { tls: true } => (DOT1X, vec![("identity", user.to_owned()), ("private-key-password", password)]),
            Wanted::Mobile { cdma } => (if cdma { CDMA } else { GSM }, vec![("password", password)]),
            Wanted::Pin => (GSM, vec![("pin", password)]),
            Wanted::Pppoe => (PPPOE, vec![("username", user.to_owned()), ("password", password)]),
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

/// `hints` NetworkManager sends that ask for the SIM PIN.
fn wants_pin(hints: &[String]) -> bool {
    hints.iter().any(|h| h == "pin")
}

/// What to ask for, for `setting_name` of a connection (`hints`: what
/// NetworkManager says it misses).
pub fn wanted(settings: &Settings, setting_name: &str, hints: &[String]) -> Option<Need> {
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
        GSM if wants_pin(hints) => (Wanted::Pin, String::new()),
        GSM => (Wanted::Mobile { cdma: false }, String::new()),
        CDMA => (Wanted::Mobile { cdma: true }, String::new()),
        PPPOE => (Wanted::Pppoe, text(settings, PPPOE, "username").unwrap_or_default()),
        _ => return None,
    };
    let wired = !settings.contains_key("802-11-wireless") && !settings.contains_key(GSM) && !settings.contains_key(CDMA);
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
        Wanted::Mobile { .. } => (format!("A password is needed to connect to “{}”.", need.name), "Password"),
        Wanted::Pin => (format!("The SIM card's PIN is needed to connect to “{}”.", need.name), "PIN"),
        Wanted::Pppoe => (format!("A user name and password are needed to connect to “{}”.", need.name), "Password"),
    };
    let title = match need.wanted {
        Wanted::Mobile { .. } | Wanted::Pin => "Mobile Broadband",
        Wanted::Pppoe => "DSL Connection",
        _ if need.wired => "Wired Network",
        _ => "Wi-Fi Network",
    };
    Prompt {
        title: title.into(),
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
        hints: Vec<String>,
        flags: u32,
    ) -> Result<HashMap<String, HashMap<String, OwnedValue>>, AgentError> {
        if setting_name == VPN {
            return self.vpn(&connection, connection_path, hints, flags).await;
        }
        if setting_name == WIREGUARD {
            return self.wireguard(&connection, connection_path, hints, flags).await;
        }
        let Some(need) = wanted(&connection, &setting_name, &hints) else {
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
                    warning = if need.wanted == Wanted::Pin { "A PIN has 4 to 8 digits.".into() } else { "This password is not valid.".into() };
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

/// A WireGuard secret to ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WgSecret {
    PrivateKey,
    /// A peer's preshared key, by the peer's public key.
    Preshared(String),
}

/// What NetworkManager's hints ask for (`private-key`,
/// `peers.<public key>.preshared-key`); none means the private key.
pub fn wireguard_wanted(hints: &[String]) -> Vec<WgSecret> {
    let mut out: Vec<WgSecret> = hints
        .iter()
        .filter_map(|h| {
            let h = h.strip_prefix("wireguard.").unwrap_or(h);
            if h == "private-key" {
                Some(WgSecret::PrivateKey)
            } else {
                h.strip_prefix("peers.")?.strip_suffix(".preshared-key").map(|k| WgSecret::Preshared(k.to_owned()))
            }
        })
        .collect();
    if out.is_empty() {
        out.push(WgSecret::PrivateKey);
    }
    out
}

/// A WireGuard key: 32 bytes in base64 (43 characters and `=`).
pub fn valid_wg_key(key: &str) -> bool {
    key.len() == 44 && key.ends_with('=') && key[..43].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

impl Agent {
    /// A WireGuard connection's private key or peers' preshared keys.
    async fn wireguard(&self, connection: &Settings, path: OwnedObjectPath, hints: Vec<String>, flags: u32) -> Result<HashMap<String, HashMap<String, OwnedValue>>, AgentError> {
        if flags & ALLOW_INTERACTION == 0 {
            return Err(AgentError::NoSecrets("no interaction allowed".into()));
        }
        let name = text(connection, "connection", "id").unwrap_or_else(|| "WireGuard".into());
        let wanted = wireguard_wanted(&hints);
        tracing::info!(name, asked = wanted.len(), "WireGuard secrets asked");
        let key = (path.to_string(), WIREGUARD.to_owned());
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), cancel_tx);
        let _turn = prompt::turn().await;
        let mut answers: Vec<(WgSecret, String)> = Vec::new();
        let mut result = Ok(());
        'secrets: for secret in &wanted {
            let message = match secret {
                WgSecret::PrivateKey => format!("The private key is needed to connect to “{name}”."),
                WgSecret::Preshared(peer) => format!("The preshared key for peer {}… is needed to connect to “{name}”.", peer.chars().take(8).collect::<String>()),
            };
            let mut warning = if flags & REQUEST_NEW != 0 { "Wrong key? Try again.".to_owned() } else { String::new() };
            loop {
                let prompt = Prompt {
                    title: "WireGuard Connection".into(),
                    message: message.clone(),
                    password: Some(if *secret == WgSecret::PrivateKey { "Private key".into() } else { "Preshared key".into() }),
                    warning: warning.clone(),
                    continue_label: "Connect".into(),
                    cancel_label: "Cancel".into(),
                    ..Prompt::default()
                };
                let answer = tokio::select! {
                    a = prompt::ask(&self.ui, prompt) => a,
                    _ = &mut cancel_rx => {
                        result = Err(AgentError::AgentCanceled("canceled by NetworkManager".into()));
                        break 'secrets;
                    }
                };
                let Answer::Continue { password: Some(p), .. } = answer else {
                    result = Err(AgentError::UserCanceled("canceled".into()));
                    break 'secrets;
                };
                let value = String::from_utf8_lossy(p.bytes()).trim().to_owned();
                if valid_wg_key(&value) {
                    answers.push((secret.clone(), value));
                    break;
                }
                warning = "This key is not valid.".into();
            }
        }
        prompt::done(&self.ui);
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        result?;
        let ov = |v: Value<'_>| OwnedValue::try_from(v).map_err(|e| AgentError::ZBus(e.into()));
        let mut group: HashMap<String, OwnedValue> = HashMap::new();
        let mut peers: Vec<HashMap<String, Value<'static>>> = Vec::new();
        for (secret, value) in answers {
            match secret {
                WgSecret::PrivateKey => {
                    group.insert("private-key".into(), ov(Value::from(value))?);
                }
                WgSecret::Preshared(peer) => peers.push(HashMap::from([("public-key".to_owned(), Value::from(peer)), ("preshared-key".to_owned(), Value::from(value))])),
            }
        }
        if !peers.is_empty() {
            group.insert("peers".into(), ov(Value::from(peers))?);
        }
        tracing::info!(name, "WireGuard secrets");
        Ok(HashMap::from([(WIREGUARD.to_owned(), group)]))
    }

    /// A VPN's secrets, through its plugin's auth dialog.
    async fn vpn(&self, connection: &Settings, path: OwnedObjectPath, hints: Vec<String>, flags: u32) -> Result<HashMap<String, HashMap<String, OwnedValue>>, AgentError> {
        use crate::vpn_secrets as v;
        let vpn = connection.get(VPN);
        let service = text(connection, VPN, "service-type").ok_or_else(|| AgentError::NoSecrets("no VPN service type".into()))?;
        let plugin = v::plugin_for(&service)
            .filter(|p| v::runnable(&p.auth_dialog))
            .ok_or_else(|| AgentError::NoSecrets(format!("no auth dialog for {service}")))?;
        let uuid = text(connection, "connection", "uuid").unwrap_or_default();
        let name = text(connection, "connection", "id").unwrap_or_else(|| service.clone());
        let interactive = flags & ALLOW_INTERACTION != 0;
        let external = plugin.external_ui && interactive;
        let args = v::dialog_args(&plugin, &uuid, &name, &service, interactive, flags & REQUEST_NEW != 0, &hints);
        let input = v::dialog_input(&v::string_dict(vpn, "data"), &v::string_dict(vpn, "secrets"));
        tracing::info!(name, service, external, interactive, "VPN secrets asked");
        let key = (path.to_string(), VPN.to_owned());
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), cancel_tx);
        let canceled = || AgentError::AgentCanceled("canceled by NetworkManager".into());
        let result = async {
            let out = tokio::select! {
                out = v::run_dialog(&plugin, &args, &input, external) => out.map_err(AgentError::NoSecrets)?,
                _ = &mut cancel_rx => return Err(canceled()),
            };
            if !external {
                // The plugin asked itself (or looked up saved secrets).
                let pairs = v::parse_pairs(&out);
                if pairs.is_empty() {
                    return Err(if interactive { AgentError::UserCanceled("canceled".into()) } else { AgentError::NoSecrets("no saved VPN secrets".into()) });
                }
                return Ok(pairs);
            }
            let mut described = v::parse_described(&out).ok_or_else(|| AgentError::NoSecrets("the auth dialog's answer is unreadable".into()))?;
            let pages = v::pages(&described.fields);
            if !pages.is_empty() {
                let _turn = prompt::turn().await;
                for page in &pages {
                    let ask = prompt::ask(&self.ui, v::page_prompt(&described, &described.fields, page, &name));
                    let answer = tokio::select! {
                        a = ask => a,
                        _ = &mut cancel_rx => {
                            prompt::done(&self.ui);
                            return Err(canceled());
                        }
                    };
                    if !v::take_answer(&mut described.fields, page, answer) {
                        prompt::done(&self.ui);
                        return Err(AgentError::UserCanceled("canceled".into()));
                    }
                }
                prompt::done(&self.ui);
            }
            Ok(v::secrets_of(&described.fields))
        }
        .await;
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        tracing::info!(name, answered = result.is_ok(), "VPN secrets");
        let secrets: HashMap<String, String> = result?.into_iter().collect();
        let secrets = OwnedValue::try_from(Value::from(secrets)).map_err(|e| AgentError::ZBus(e.into()))?;
        Ok(HashMap::from([(VPN.to_owned(), HashMap::from([("secrets".to_owned(), secrets)]))]))
    }
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
        let need = |s: &Settings, name: &str| wanted(s, name, &[]).map(|n| n.wanted);
        let home = wanted(&settings("wpa-psk"), SECURITY, &[]).unwrap();
        assert_eq!((home.wanted, home.name.as_str(), home.wired), (Wanted::Psk, "HomeNet", false));
        assert_eq!(need(&settings("sae"), SECURITY), Some(Wanted::Psk));
        assert_eq!(need(&settings("none"), SECURITY), Some(Wanted::Wep));
        assert_eq!(need(&settings("wpa-eap"), SECURITY), None);
        assert_eq!(need(&settings("wpa-psk"), "802-1x"), Some(Wanted::Eap { tls: false }));
        let v = |x: Value<'_>| OwnedValue::try_from(x).unwrap();
        let mut leap = settings("ieee8021x");
        leap.get_mut(SECURITY).unwrap().extend([("auth-alg".to_owned(), v(Value::from("leap"))), ("leap-username".to_owned(), v(Value::from("ada")))]);
        let n = wanted(&leap, SECURITY, &[]).unwrap();
        assert_eq!((n.wanted, n.user.as_str()), (Wanted::Leap, "ada"));
        // Wired 802.1X with TLS.
        let wired: Settings = HashMap::from([
            ("connection".to_owned(), HashMap::from([("id".to_owned(), v(Value::from("Office LAN")))])),
            (DOT1X.to_owned(), HashMap::from([("eap".to_owned(), v(Value::from(vec!["tls"]))), ("identity".to_owned(), v(Value::from("ada@example.org")))])),
        ]);
        let n = wanted(&wired, DOT1X, &[]).unwrap();
        assert_eq!(n, Need { wanted: Wanted::Eap { tls: true }, name: "Office LAN".into(), user: "ada@example.org".into(), wired: true });
        let p = prompt_for(&n, &n.user, String::new());
        assert_eq!((p.title.as_str(), p.entry, p.password.as_deref()), ("Wired Network", Some(("User name".into(), "ada@example.org".into())), Some("Private key password")));
        assert_eq!(Wanted::Eap { tls: false }.secrets("ada", "pw".into()), (DOT1X, vec![("identity", "ada".into()), ("password", "pw".into())]));
        assert_eq!(Wanted::Leap.secrets("ada", "pw".into()).0, SECURITY);
        // Mobile broadband and DSL.
        let mobile: Settings = HashMap::from([
            ("connection".to_owned(), HashMap::from([("id".to_owned(), v(Value::from("Carrier")))])),
            (GSM.to_owned(), HashMap::from([("apn".to_owned(), v(Value::from("internet")))])),
        ]);
        let n = wanted(&mobile, GSM, &[]).unwrap();
        assert_eq!((n.wanted, n.wired, prompt_for(&n, "", String::new()).title.as_str()), (Wanted::Mobile { cdma: false }, false, "Mobile Broadband"));
        assert_eq!(wanted(&mobile, GSM, &["pin".into()]).unwrap().wanted, Wanted::Pin);
        assert_eq!(Wanted::Pin.secrets("", "1234".into()), (GSM, vec![("pin", "1234".into())]));
        assert_eq!(Wanted::Mobile { cdma: true }.secrets("", "pw".into()).0, CDMA);
        let dsl: Settings = HashMap::from([(PPPOE.to_owned(), HashMap::from([("username".to_owned(), v(Value::from("ann")))]))]);
        let n = wanted(&dsl, PPPOE, &[]).unwrap();
        assert_eq!((n.wanted, n.user.as_str()), (Wanted::Pppoe, "ann"));
        assert_eq!(Wanted::Pppoe.secrets("ann", "pw".into()), (PPPOE, vec![("username", "ann".into()), ("password", "pw".into())]));
        assert_eq!(wanted(&dsl, VPN, &[]), None);
    }

    #[test]
    fn wireguard() {
        assert_eq!(wireguard_wanted(&[]), [WgSecret::PrivateKey]);
        assert_eq!(
            wireguard_wanted(&["private-key".into(), "peers.AbC+/=.preshared-key".into(), "x".into()]),
            [WgSecret::PrivateKey, WgSecret::Preshared("AbC+/=".into())]
        );
        assert!(valid_wg_key("yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk="));
        assert!(!valid_wg_key("yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk"));
        assert!(!valid_wg_key("not a key"));
    }

    #[test]
    fn password_rules() {
        assert!(!Wanted::Psk.valid("short"));
        assert!(Wanted::Psk.valid("long enough"));
        assert!(Wanted::Psk.valid(&"a".repeat(64)), "64 hex digits");
        assert!(!Wanted::Psk.valid(&"g".repeat(64)));
        assert!(Wanted::Wep.valid("abcde") && Wanted::Wep.valid("0123456789"));
        assert!(!Wanted::Wep.valid("abcdef"));
        assert!(Wanted::Pin.valid("1234") && Wanted::Pin.valid("12345678"));
        assert!(!Wanted::Pin.valid("123") && !Wanted::Pin.valid("12a4"));
    }
}
