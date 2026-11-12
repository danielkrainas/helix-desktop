//! VPN secrets for NetworkManager's secret agent (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.NetworkManager.SecretAgent.md`, "VPN
//! secrets"): each VPN plugin's auth dialog says which secrets to ask
//! for (external UI mode) or asks itself; the shell asks in its prompt.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::prompt::{Answer, Prompt};

/// Where VPN plugins describe themselves (`<name>.name` files).
pub const PLUGIN_DIRS: &[&str] = &["/etc/NetworkManager/VPN", "/usr/lib/NetworkManager/VPN", "/usr/lib64/NetworkManager/VPN"];

/// How long an auth dialog may take to say what it needs (external UI
/// mode; its own dialogs may take as long as the user does).
pub const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(20);

/// A VPN plugin's auth dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plugin {
    pub auth_dialog: PathBuf,
    /// It can describe the secrets instead of asking itself.
    pub external_ui: bool,
    /// It takes the plugin's hints (`-t`).
    pub hints: bool,
}

fn plugin_dirs() -> Vec<PathBuf> {
    // Tests point this at their own plugins.
    let mut dirs: Vec<PathBuf> = std::env::var_os("HELIX_NM_VPN_DIR").map(PathBuf::from).into_iter().collect();
    dirs.extend(PLUGIN_DIRS.iter().map(PathBuf::from));
    dirs
}

/// Parse a plugin's `.name` file: the plugin, if it serves `service`.
pub fn parse_plugin(text: &str, service: &str) -> Option<Plugin> {
    let file = gio::glib::KeyFile::new();
    file.load_from_data(text, gio::glib::KeyFileFlags::NONE).ok()?;
    let get = |g: &str, k: &str| file.string(g, k).ok().map(|s| s.to_string());
    let bool_of = |g: &str, k: &str| file.boolean(g, k).unwrap_or(false);
    let serves = get("VPN Connection", "service").as_deref() == Some(service)
        || get("VPN Connection", "name").is_some_and(|n| service == n || service.rsplit('.').next() == Some(n.as_str()));
    if !serves {
        return None;
    }
    let dialog = get("GNOME", "auth-dialog")?;
    Some(Plugin { auth_dialog: PathBuf::from(dialog), external_ui: bool_of("GNOME", "supports-external-ui-mode"), hints: bool_of("GNOME", "supports-hints") })
}

/// The plugin serving `service`, from the first directory that has one.
pub fn plugin_for(service: &str) -> Option<Plugin> {
    for dir in plugin_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "name")).collect();
        files.sort();
        for file in files {
            if let Some(plugin) = std::fs::read_to_string(&file).ok().and_then(|t| parse_plugin(&t, service)) {
                return Some(plugin);
            }
        }
    }
    None
}

/// What the auth dialog reads on standard input: the VPN's data and
/// current secrets, then `DONE`.
pub fn dialog_input(data: &BTreeMap<String, String>, secrets: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (k, v) in data {
        out.push_str(&format!("DATA_KEY={k}\nDATA_VAL={v}\n\n"));
    }
    for (k, v) in secrets {
        out.push_str(&format!("SECRET_KEY={k}\nSECRET_VAL={v}\n\n"));
    }
    out.push_str("DONE\n\n");
    out
}

/// The auth dialog's command line.
pub fn dialog_args(plugin: &Plugin, uuid: &str, name: &str, service: &str, interactive: bool, reprompt: bool, hints: &[String]) -> Vec<String> {
    let mut args = vec!["-u".to_owned(), uuid.to_owned(), "-n".to_owned(), name.to_owned(), "-s".to_owned(), service.to_owned()];
    if interactive {
        args.push("-i".into());
    }
    if reprompt {
        args.push("-r".into());
    }
    if plugin.external_ui && interactive {
        args.push("--external-ui-mode".into());
    }
    if plugin.hints {
        for hint in hints {
            args.push("-t".into());
            args.push(hint.clone());
        }
    }
    args
}

/// A secret the auth dialog described.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub key: String,
    pub value: String,
    pub label: String,
    pub secret: bool,
    /// The user is asked (otherwise `value` is used as it is).
    pub ask: bool,
}

/// The auth dialog's description (external UI mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Described {
    pub title: String,
    pub description: String,
    pub fields: Vec<Field>,
}

/// Parse the keyfile an auth dialog prints in external UI mode.
pub fn parse_described(text: &str) -> Option<Described> {
    let file = gio::glib::KeyFile::new();
    file.load_from_data(text, gio::glib::KeyFileFlags::NONE).ok()?;
    const UI: &str = "VPN Plugin UI";
    if !file.has_group(UI) {
        return None;
    }
    let get = |g: &str, k: &str| file.string(g, k).map(|s| s.to_string()).unwrap_or_default();
    let fields = file
        .groups()
        .iter()
        .map(|g| g.to_string())
        .filter(|g| g != UI)
        .map(|g| Field {
            value: get(&g, "Value"),
            label: get(&g, "Label"),
            secret: file.boolean(&g, "IsSecret").unwrap_or(false),
            ask: file.boolean(&g, "ShouldAsk").unwrap_or(false),
            key: g,
        })
        .collect();
    Some(Described { title: get(UI, "Title"), description: get(UI, "Description"), fields })
}

/// Parse the secrets an auth dialog prints when it asked itself: a key
/// line and a value line each, up to an empty line.
pub fn parse_pairs(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut lines = text.lines();
    while let Some(key) = lines.next() {
        if key.is_empty() {
            break;
        }
        let Some(value) = lines.next() else { break };
        out.insert(key.to_owned(), value.to_owned());
    }
    out
}

/// The fields to ask, a dialog's worth at a time: at most one plain text
/// field and two secret ones each, in the order described.
pub fn pages(fields: &[Field]) -> Vec<Vec<usize>> {
    let mut pages: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let (mut texts, mut secrets) = (0, 0);
    for (i, f) in fields.iter().enumerate().filter(|(_, f)| f.ask) {
        let full = if f.secret { secrets == 2 } else { texts == 1 || secrets > 0 };
        if full && !current.is_empty() {
            pages.push(std::mem::take(&mut current));
            (texts, secrets) = (0, 0);
        }
        if f.secret {
            secrets += 1;
        } else {
            texts += 1;
        }
        current.push(i);
    }
    if !current.is_empty() {
        pages.push(current);
    }
    pages
}

/// The dialog for one page of fields.
pub fn page_prompt(described: &Described, fields: &[Field], page: &[usize], name: &str) -> Prompt {
    let mut prompt = Prompt {
        title: if described.title.is_empty() { "VPN Connection".into() } else { described.title.clone() },
        message: if described.description.is_empty() { format!("A password is needed to connect to “{name}”.") } else { described.description.clone() },
        continue_label: "Connect".into(),
        cancel_label: "Cancel".into(),
        ..Prompt::default()
    };
    for &i in page {
        let f = &fields[i];
        let label = if f.label.is_empty() { f.key.clone() } else { f.label.clone() };
        if !f.secret {
            prompt.entry = Some((label, f.value.clone()));
        } else if prompt.password.is_none() {
            prompt.password = Some(label);
        } else {
            prompt.confirm = Some(label);
        }
    }
    prompt
}

/// Put a page's answer into `fields`; false if cancelled.
pub fn take_answer(fields: &mut [Field], page: &[usize], answer: Answer) -> bool {
    let Answer::Continue { entry, password, confirm, .. } = answer else { return false };
    let mut secrets = [password, confirm].into_iter();
    for &i in page {
        if fields[i].secret {
            if let Some(Some(s)) = secrets.next() {
                fields[i].value = String::from_utf8_lossy(s.bytes()).into_owned();
            }
        } else if let Some(text) = &entry {
            fields[i].value = text.trim().to_owned();
        }
    }
    true
}

/// The secrets to hand NetworkManager: the fields asked, and those not
/// asked that have a value (such as the placeholder `no-secret`).
pub fn secrets_of(fields: &[Field]) -> BTreeMap<String, String> {
    fields.iter().filter(|f| f.ask || !f.value.is_empty()).map(|f| (f.key.clone(), f.value.clone())).collect()
}

/// Run the auth dialog: its standard output once it is done.
///
/// In external UI mode it prints its description and exits; otherwise it
/// prints the secrets once the user answered its own dialog, and is then
/// told to quit.
pub async fn run_dialog(plugin: &Plugin, args: &[String], input: &str, external: bool) -> Result<String, String> {
    let mut child = tokio::process::Command::new(&plugin.auth_dialog)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", plugin.auth_dialog.display()))?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    stdin.write_all(input.as_bytes()).await.map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let read = async {
        let mut buf = [0u8; 4096];
        loop {
            let n = stdout.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                return Ok::<_, String>(());
            }
            out.extend_from_slice(&buf[..n]);
            // Without external UI, the secrets end with an empty line.
            if !external && (out.starts_with(b"\n") || out.windows(2).any(|w| w == b"\n\n")) {
                return Ok(());
            }
        }
    };
    if external {
        tokio::time::timeout(DESCRIBE_TIMEOUT, read).await.map_err(|_| "the auth dialog did not answer".to_owned())??;
    } else {
        read.await?;
    }
    let _ = stdin.write_all(b"QUIT\n\n").await;
    drop(stdin);
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// The `vpn` setting's string dictionaries (`data`, `secrets`).
pub fn string_dict(group: Option<&HashMap<String, zbus::zvariant::OwnedValue>>, key: &str) -> BTreeMap<String, String> {
    group
        .and_then(|g| g.get(key))
        .and_then(|v| HashMap::<String, String>::try_from(v.clone()).ok())
        .map(|m| m.into_iter().collect())
        .unwrap_or_default()
}

/// Whether `path` can be run.
pub fn runnable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &str = "[VPN Connection]\nname=openvpn\nservice=org.freedesktop.NetworkManager.openvpn\n\n[GNOME]\nauth-dialog=/usr/libexec/nm-openvpn-auth-dialog\nsupports-external-ui-mode=true\nsupports-hints=true\n";

    #[test]
    fn plugins() {
        let p = parse_plugin(NAME, "org.freedesktop.NetworkManager.openvpn").unwrap();
        assert_eq!(p, Plugin { auth_dialog: "/usr/libexec/nm-openvpn-auth-dialog".into(), external_ui: true, hints: true });
        assert!(parse_plugin(NAME, "org.freedesktop.NetworkManager.pptp").is_none());
        assert!(parse_plugin(NAME, "openvpn").is_some());
        assert!(parse_plugin("[VPN Connection]\nservice=x\n", "x").is_none());
        let args = dialog_args(&p, "u", "Work", "org.freedesktop.NetworkManager.openvpn", true, true, &["x-vpn-message:Hi".into()]);
        assert_eq!(args.join(" "), "-u u -n Work -s org.freedesktop.NetworkManager.openvpn -i -r --external-ui-mode -t x-vpn-message:Hi");
        let args = dialog_args(&p, "u", "Work", "s", false, false, &[]);
        assert_eq!(args.join(" "), "-u u -n Work -s s");
    }

    #[test]
    fn exchange() {
        let data = BTreeMap::from([("connection-type".to_owned(), "password".to_owned())]);
        let secrets = BTreeMap::from([("password".to_owned(), "old".to_owned())]);
        assert_eq!(dialog_input(&data, &secrets), "DATA_KEY=connection-type\nDATA_VAL=password\n\nSECRET_KEY=password\nSECRET_VAL=old\n\nDONE\n\n");
        let text = "[VPN Plugin UI]\nVersion=2\nDescription=Authenticate to “Work”.\nTitle=Authentication required\n\n[password]\nValue=\nLabel=Password\nIsSecret=true\nShouldAsk=true\n\n[cert-pass]\nValue=\nLabel=Certificate password\nIsSecret=true\nShouldAsk=false\n";
        let d = parse_described(text).unwrap();
        assert_eq!(d.title, "Authentication required");
        assert_eq!(d.fields.iter().map(|f| (f.key.as_str(), f.ask)).collect::<Vec<_>>(), [("password", true), ("cert-pass", false)]);
        assert!(parse_described("[Other]\nA=b\n").is_none());
        assert_eq!(parse_pairs("password\nsecret\nuser\nann\n\n\n"), BTreeMap::from([("password".to_owned(), "secret".to_owned()), ("user".to_owned(), "ann".to_owned())]));
        assert!(parse_pairs("\n\n").is_empty());
    }

    fn field(key: &str, secret: bool, ask: bool) -> Field {
        Field { key: key.into(), value: String::new(), label: key.into(), secret, ask }
    }

    #[test]
    fn paging() {
        let f = [field("user", false, true), field("pass", true, true), field("otp", true, true), field("key", true, true), field("skip", true, false), field("domain", false, true)];
        assert_eq!(pages(&f), vec![vec![0, 1, 2], vec![3], vec![5]]);
        let d = Described { title: String::new(), description: String::new(), fields: f.to_vec() };
        let p = page_prompt(&d, &f, &[0, 1, 2], "Work");
        assert_eq!((p.entry.map(|e| e.0), p.password, p.confirm), (Some("user".into()), Some("pass".into()), Some("otp".into())));
        assert_eq!(p.title, "VPN Connection");
        assert!(pages(&[field("a", true, false)]).is_empty());
        let mut kept = f.to_vec();
        kept[4].value = "abc".into();
        assert_eq!(secrets_of(&kept).keys().map(String::as_str).collect::<Vec<_>>(), ["domain", "key", "otp", "pass", "skip", "user"]);
        assert_eq!(secrets_of(&f).len(), 5);
    }
}
