//! The polkit authentication agent (helix-desktop-spec
//! `org.freedesktop.PolicyKit1.AuthenticationAgent.md`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use zbus::message::Header;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{interface, Connection};

use crate::prompt::{self, Answer, Prompt};
use crate::UiEvent;

pub const AGENT_PATH: &str = "/org/helix/PolicyKit1/AuthenticationAgent";
const POLKIT: &str = "org.freedesktop.PolicyKit1";
const AUTHORITY_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";
const AUTHORITY: &str = "org.freedesktop.PolicyKit1.Authority";
const HELPERS: [&str; 2] = ["/usr/lib/polkit-1/polkit-agent-helper-1", "/usr/libexec/polkit-agent-helper-1"];
const RETRY: &str = "Sorry, that didn't work. Please try again.";

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.PolicyKit1.Error")]
pub enum PolkitError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Failed(String),
    Cancelled(String),
    NotAuthorized(String),
}

/// polkit's setuid helper (`HELIX_POLKIT_HELPER` overrides it, for tests).
pub fn helper() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("HELIX_POLKIT_HELPER") {
        return Some(PathBuf::from(path));
    }
    HELPERS.iter().map(PathBuf::from).find(|p| p.exists())
}

/// A user: (login name, real name).
fn user(uid: u32) -> Option<(String, String)> {
    let mut buf = vec![0u8; 16384];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwuid_r fills `pwd` with pointers into `buf`, which outlives
    // the reads below.
    let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr().cast(), buf.len(), &mut result) };
    if rc != 0 || result.is_null() || pwd.pw_name.is_null() {
        return None;
    }
    // SAFETY: NUL-terminated strings in `buf`.
    let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) }.to_string_lossy().into_owned();
    let gecos = if pwd.pw_gecos.is_null() {
        String::new()
    } else {
        // SAFETY: as above.
        unsafe { std::ffi::CStr::from_ptr(pwd.pw_gecos) }.to_string_lossy().split(',').next().unwrap_or("").to_owned()
    };
    Some((name, gecos))
}

/// The users of group `gid`: its listed members (by name), looked up.
pub fn group_members(gid: u32) -> Vec<u32> {
    let mut buf = vec![0u8; 65536];
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::group = std::ptr::null_mut();
    // SAFETY: getgrgid_r fills `grp` with pointers into `buf`, which
    // outlives the reads below.
    let rc = unsafe { libc::getgrgid_r(gid, &mut grp, buf.as_mut_ptr().cast(), buf.len(), &mut result) };
    if rc != 0 || result.is_null() || grp.gr_mem.is_null() {
        return Vec::new();
    }
    let mut names = Vec::new();
    // SAFETY: gr_mem is a NULL-terminated array of NUL-terminated strings
    // in `buf`.
    unsafe {
        let mut p = grp.gr_mem;
        while !(*p).is_null() {
            names.push(std::ffi::CStr::from_ptr(*p).to_owned());
            p = p.add(1);
        }
    }
    names.iter().filter_map(|n| uid_of(n)).collect()
}

/// The uid of login name `name`.
fn uid_of(name: &std::ffi::CStr) -> Option<u32> {
    let mut buf = vec![0u8; 16384];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: as in `user`.
    let rc = unsafe { libc::getpwnam_r(name.as_ptr(), &mut pwd, buf.as_mut_ptr().cast(), buf.len(), &mut result) };
    (rc == 0 && !result.is_null()).then_some(pwd.pw_uid)
}

/// The groups the session's user is in (primary and supplementary).
pub fn own_groups() -> Vec<u32> {
    // SAFETY: getgid cannot fail; getgroups with 0 asks for the count, then
    // fills at most that many.
    unsafe {
        let mut groups = vec![libc::getgid()];
        let n = libc::getgroups(0, std::ptr::null_mut());
        if n > 0 {
            let mut more = vec![0 as libc::gid_t; n as usize];
            let got = libc::getgroups(n, more.as_mut_ptr());
            if got > 0 {
                groups.extend(more.into_iter().take(got as usize));
            }
        }
        groups
    }
}

fn id_of(details: &HashMap<String, OwnedValue>, key: &str) -> Option<u32> {
    match details.get(key).map(|v| &**v) {
        Some(Value::U32(id)) => Some(*id),
        _ => None,
    }
}

/// Everyone who may authenticate: the session's user first when allowed
/// (offered, or in an offered group), then the offered users, then the
/// offered groups' members; each once, at most `MAX_USERS`.
pub fn candidates(identities: &[(String, HashMap<String, OwnedValue>)], own: u32, own_groups: &[u32], members: impl Fn(u32) -> Vec<u32>) -> Vec<u32> {
    let of = |kind: &str, key: &str| -> Vec<u32> { identities.iter().filter(|(k, _)| k == kind).filter_map(|(_, d)| id_of(d, key)).collect() };
    let (uids, gids) = (of("unix-user", "uid"), of("unix-group", "gid"));
    let mut out = Vec::new();
    if uids.contains(&own) || gids.iter().any(|g| own_groups.contains(g)) {
        out.push(own);
    }
    for uid in uids.into_iter().chain(gids.iter().flat_map(|g| members(*g))) {
        if !out.contains(&uid) && out.len() < MAX_USERS {
            out.push(uid);
        }
    }
    out
}

/// Most users offered in the chooser.
pub const MAX_USERS: usize = 20;

/// How a user is listed: "Real Name (login)", or the login alone.
pub fn user_label(login: &str, real: &str) -> String {
    if real.trim().is_empty() { login.to_owned() } else { format!("{} ({login})", real.trim()) }
}

/// A helper prompt as a field label ("Password: " → "Password").
pub fn field_label(prompt: &str) -> String {
    let label = prompt.trim_end().trim_end_matches(':').trim();
    if label.is_empty() { "Password".into() } else { label.to_owned() }
}

/// The served agent.
pub struct Agent {
    ui: mpsc::UnboundedSender<UiEvent>,
    /// Running authentications by cookie: how to cancel them.
    running: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
}

impl Agent {
    pub fn new(ui: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self { ui, running: Default::default() }
    }
}

/// Only polkitd may call the agent.
async fn check_caller(conn: &Connection, header: &Header<'_>) -> Result<(), PolkitError> {
    let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
    let dbus = zbus::fdo::DBusProxy::new(conn).await?;
    let name = zbus::names::BusName::try_from(POLKIT).map_err(|e| PolkitError::Failed(e.to_string()))?;
    match dbus.get_name_owner(name).await {
        Ok(owner) if owner.as_str() == sender => Ok(()),
        _ => Err(PolkitError::NotAuthorized("only polkit may ask".into())),
    }
}

/// Run the helper until it succeeds, the user cancels, or `cancel` fires.
async fn authenticate(ui: &mpsc::UnboundedSender<UiEvent>, message: &str, cookie: &str, name: &str, real: &str, mut cancel: oneshot::Receiver<()>) -> Result<(), PolkitError> {
    let helper = helper().ok_or_else(|| PolkitError::Failed("polkit-agent-helper-1 not found".into()))?;
    let shown = if real.is_empty() { name.to_owned() } else { real.to_owned() };
    let mut warning = String::new();
    loop {
        let mut child = tokio::process::Command::new(&helper)
            .arg(name)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| PolkitError::Failed(format!("cannot run the polkit helper: {e}")))?;
        let mut stdin = child.stdin.take().ok_or_else(|| PolkitError::Failed("no helper input".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| PolkitError::Failed("no helper output".into()))?;
        stdin.write_all(format!("{cookie}\n").as_bytes()).await.map_err(|e| PolkitError::Failed(e.to_string()))?;
        let mut lines = BufReader::new(stdout).lines();
        let mut info = String::new();
        let succeeded = loop {
            let line = tokio::select! {
                _ = &mut cancel => {
                    let _ = child.kill().await;
                    prompt::done(ui);
                    return Err(PolkitError::Cancelled("cancelled by polkit".into()));
                }
                line = lines.next_line() => line,
            };
            let Ok(Some(line)) = line else { break false };
            if line == "SUCCESS" {
                break true;
            }
            if line == "FAILURE" {
                break false;
            }
            let (kind, text) = line.split_once(' ').unwrap_or((line.as_str(), ""));
            match kind {
                "PAM_PROMPT_ECHO_OFF" | "PAM_PROMPT_ECHO_ON" => {
                    let p = Prompt {
                        title: "Authentication Required".into(),
                        message: message.into(),
                        description: if info.is_empty() { format!("Authenticating as {shown}") } else { info.clone() },
                        password: Some(field_label(text)),
                        warning: std::mem::take(&mut warning),
                        continue_label: "Authenticate".into(),
                        cancel_label: "Cancel".into(),
                        ..Prompt::default()
                    };
                    let answer = tokio::select! {
                        _ = &mut cancel => {
                            let _ = child.kill().await;
                            prompt::done(ui);
                            return Err(PolkitError::Cancelled("cancelled by polkit".into()));
                        }
                        answer = prompt::ask(ui, p) => answer,
                    };
                    match answer {
                        Answer::Continue { password, .. } => {
                            let mut reply = password.map(|p| p.bytes().to_vec()).unwrap_or_default();
                            reply.push(b'\n');
                            let written = stdin.write_all(&reply).await;
                            reply.iter_mut().for_each(|b| *b = 0);
                            if written.is_err() {
                                break false;
                            }
                        }
                        Answer::Cancel => {
                            let _ = child.kill().await;
                            prompt::done(ui);
                            return Err(PolkitError::Cancelled("cancelled by the user".into()));
                        }
                    }
                }
                "PAM_ERROR_MSG" => warning = text.to_owned(),
                "PAM_TEXT_INFO" => info = text.to_owned(),
                _ => tracing::debug!(line, "unknown polkit helper output"),
            }
        };
        let _ = child.wait().await;
        if succeeded {
            prompt::done(ui);
            return Ok(());
        }
        if warning.is_empty() {
            warning = RETRY.into();
        }
    }
}

#[interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl Agent {
    #[allow(clippy::too_many_arguments)]
    async fn begin_authentication(
        &self,
        action_id: &str,
        message: &str,
        _icon_name: &str,
        _details: HashMap<String, String>,
        cookie: &str,
        identities: Vec<(String, HashMap<String, OwnedValue>)>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<(), PolkitError> {
        check_caller(conn, &header).await?;
        // SAFETY: getuid cannot fail.
        let own = unsafe { libc::getuid() };
        let users: Vec<(u32, String, String)> = candidates(&identities, own, &own_groups(), group_members)
            .into_iter()
            .filter_map(|uid| user(uid).map(|(name, real)| (uid, name, real)))
            .collect();
        if users.is_empty() {
            return Err(PolkitError::Failed("no user to authenticate as".into()));
        }
        let (cancel_tx, mut cancel) = oneshot::channel();
        self.running.lock().unwrap_or_else(|e| e.into_inner()).insert(cookie.to_owned(), cancel_tx);
        let _turn = prompt::turn().await;
        // Several may: the user picks first.
        let chosen = if users.len() == 1 {
            Some(0)
        } else {
            let choose = prompt::Prompt {
                title: "Authentication Required".into(),
                message: message.to_owned(),
                description: "Choose the user to authenticate as.".into(),
                list: users.iter().map(|(_, name, real)| user_label(name, real)).collect(),
                selected: 0,
                activate_rows: true,
                continue_label: "Next".into(),
                cancel_label: "Cancel".into(),
                ..Default::default()
            };
            tokio::select! {
                answer = prompt::ask(&self.ui, choose) => match answer {
                    prompt::Answer::Continue { selected, .. } => Some(selected.min(users.len() - 1)),
                    prompt::Answer::Cancel => None,
                },
                _ = &mut cancel => {
                    prompt::done(&self.ui);
                    None
                }
            }
        };
        let Some(chosen) = chosen else {
            self.running.lock().unwrap_or_else(|e| e.into_inner()).remove(cookie);
            tracing::info!(action_id, "authentication canceled while choosing the user");
            return Err(PolkitError::Cancelled("canceled".into()));
        };
        let (_, name, real) = &users[chosen];
        tracing::info!(action_id, user = name, choices = users.len(), "authentication requested");
        let result = authenticate(&self.ui, message, cookie, name, real, cancel).await;
        self.running.lock().unwrap_or_else(|e| e.into_inner()).remove(cookie);
        tracing::info!(action_id, ok = result.is_ok(), "authentication finished");
        result
    }

    async fn cancel_authentication(&self, cookie: &str, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> Result<(), PolkitError> {
        check_caller(conn, &header).await?;
        if let Some(cancel) = self.running.lock().unwrap_or_else(|e| e.into_inner()).remove(cookie) {
            let _ = cancel.send(());
        }
        Ok(())
    }
}

/// Our logind session's id.
async fn session_id(system: &Connection) -> zbus::Result<String> {
    let reply = system
        .call_method(Some("org.freedesktop.login1"), "/org/freedesktop/login1", Some("org.freedesktop.login1.Manager"), "GetSessionByPID", &(std::process::id(),))
        .await?;
    let path: OwnedObjectPath = reply.body().deserialize()?;
    let proxy = zbus::Proxy::new(system, "org.freedesktop.login1", path, "org.freedesktop.login1.Session").await?;
    proxy.get_property("Id").await
}

fn locale() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "C".into())
}

/// Serve the agent on the system bus and register it for our session.
/// The returned connection keeps it registered.
pub async fn register(ui: mpsc::UnboundedSender<UiEvent>) -> zbus::Result<Connection> {
    let system = zbus::connection::Builder::system()?.serve_at(AGENT_PATH, Agent::new(ui))?.build().await?;
    let id = session_id(&system).await?;
    let subject = ("unix-session", HashMap::from([("session-id", Value::from(id.clone()))]));
    system.call_method(Some(POLKIT), AUTHORITY_PATH, Some(AUTHORITY), "RegisterAuthenticationAgent", &(subject, locale(), AGENT_PATH)).await?;
    tracing::info!(session = id, "polkit authentication agent registered");
    Ok(system)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(kind: &str, key: &str, id: u32) -> (String, HashMap<String, OwnedValue>) {
        (kind.into(), HashMap::from([(key.into(), OwnedValue::from(id))]))
    }

    fn first_candidate(identities: &[(String, HashMap<String, OwnedValue>)], own: u32, own_groups: &[u32], members: impl Fn(u32) -> Vec<u32>) -> Option<u32> {
        candidates(identities, own, own_groups, members).first().copied()
    }

    #[test]
    fn identities() {
        let none = |_| Vec::new();
        let ids = vec![identity("unix-group", "gid", 27), identity("unix-user", "uid", 0), identity("unix-user", "uid", 1000)];
        assert_eq!(first_candidate(&ids, 1000, &[1000], none), Some(1000), "ourselves first");
        assert_eq!(first_candidate(&ids, 1001, &[1001], none), Some(0), "else the first user");
        let wheel = [identity("unix-group", "gid", 27)];
        assert_eq!(first_candidate(&wheel, 1000, &[1000, 27], none), Some(1000), "a member of the group");
        assert_eq!(first_candidate(&wheel, 1000, &[1000], |g| if g == 27 { vec![1002, 1003] } else { Vec::new() }), Some(1002), "else a member");
        assert_eq!(first_candidate(&wheel, 1000, &[1000], none), None, "an empty group");
        let both = vec![identity("unix-user", "uid", 0), identity("unix-group", "gid", 27), identity("unix-user", "uid", 1000)];
        assert_eq!(candidates(&both, 1000, &[1000], |_| vec![1002, 1000]), [1000, 0, 1002], "ours first, then users, then members, once each");
        assert_eq!(candidates(&both, 1001, &[1001], none), [0, 1000]);
        assert!(candidates(&[identity("unix-group", "gid", 27)], 1000, &[1000], none).is_empty());
        assert_eq!(user_label("ann", "Ann Lee"), "Ann Lee (ann)");
        assert_eq!(user_label("root", ""), "root");
        // The real lookup: root's group lists nobody, usually; it must not fail.
        let _ = group_members(0);
        assert!(own_groups().contains(&unsafe { libc::getgid() }));
    }

    #[test]
    fn labels() {
        assert_eq!(field_label("Password: "), "Password");
        assert_eq!(field_label("Passwort:"), "Passwort");
        assert_eq!(field_label(""), "Password");
    }
}
