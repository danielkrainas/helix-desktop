//! Portal backends that ask the user through the shell
//! (helix-desktop-spec `contracts/dbus/org.freedesktop.impl.portal.md`,
//! "Access", "Account", "AppChooser", "Background"), over the shell's
//! `org.helix.PortalDialog`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use gio::prelude::*;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::{fdo, interface, Connection, ObjectServer};

use crate::screenshot::{response, with_request, Results};

const SHELL: &str = "org.gnome.Shell";
const DIALOG_PATH: &str = "/org/helix/PortalDialog";
const DIALOG: &str = "org.helix.PortalDialog";

pub(crate) fn value(v: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(v).expect("plain values convert")
}

fn opt_str(options: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    options.get(key).and_then(|v| String::try_from(v.clone()).ok()).filter(|s| !s.is_empty())
}

/// The name of the app `app_id` from its desktop file, else the id.
pub fn app_name(app_id: &str) -> String {
    let file = format!("{app_id}.desktop");
    gio::AppInfo::all().into_iter().find(|a| a.id().is_some_and(|id| id == file)).map_or_else(|| app_id.to_owned(), |a| a.display_name().to_string())
}

#[allow(clippy::too_many_arguments)]
async fn ask(conn: &Connection, title: &str, message: &str, description: &str, grant: &str, deny: &str, check_label: &str, check: bool) -> (bool, bool) {
    let reply = conn.call_method(Some(SHELL), DIALOG_PATH, Some(DIALOG), "Ask", &(title, message, description, grant, deny, check_label, check)).await;
    match reply.and_then(|r| r.body().deserialize::<(bool, bool)>()) {
        Ok(answer) => answer,
        Err(err) => {
            tracing::warn!(%err, "the shell cannot ask");
            (false, check)
        }
    }
}

/// Show the shell's general dialog (`org.helix.PortalDialog.Dialog`);
/// its results when accepted.
pub(crate) async fn dialog(conn: &Connection, options: HashMap<String, OwnedValue>) -> Option<HashMap<String, OwnedValue>> {
    let reply = conn.call_method(Some(SHELL), DIALOG_PATH, Some(DIALOG), "Dialog", &(options,)).await;
    match reply.and_then(|r| r.body().deserialize::<(u32, HashMap<String, OwnedValue>)>()) {
        Ok((0, results)) => Some(results),
        Ok(_) => None,
        Err(err) => {
            tracing::warn!(%err, "the shell cannot show a dialog");
            None
        }
    }
}

// ----- Access ----------------------------------------------------------

/// A choice of `AccessDialog`: id, label, options, initial value.
pub type Choice = (String, String, Vec<(String, String)>, String);

/// The check box to show (the first choice without options): its index.
pub fn check_box(choices: &[Choice]) -> Option<usize> {
    choices.iter().position(|(_, _, options, _)| options.is_empty())
}

/// The answered choices: every initial value, the check box's as checked.
pub fn answered_choices(choices: &[Choice], check: Option<(usize, bool)>) -> Vec<(String, String)> {
    choices
        .iter()
        .enumerate()
        .map(|(i, (id, _, _, initial))| match check {
            Some((at, checked)) if at == i => (id.clone(), checked.to_string()),
            _ => (id.clone(), initial.clone()),
        })
        .collect()
}

pub struct AccessBackend {
    pub conn: Connection,
}

#[interface(name = "org.freedesktop.impl.portal.Access")]
impl AccessBackend {
    #[allow(clippy::too_many_arguments)]
    async fn access_dialog(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: ObjectPath<'_>,
        app_id: String,
        _parent_window: String,
        title: String,
        subtitle: String,
        body: String,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        let grant = opt_str(&options, "grant_label").unwrap_or_else(|| "Allow".into());
        let deny = opt_str(&options, "deny_label").unwrap_or_else(|| "Deny".into());
        let choices: Vec<Choice> = options.get("choices").and_then(|v| Vec::<Choice>::try_from(v.clone()).ok()).unwrap_or_default();
        let check = check_box(&choices);
        let (label, initial) = check.map_or((String::new(), false), |i| (choices[i].1.clone(), choices[i].3 == "true"));
        tracing::info!(app_id, title, "access dialog");
        let conn = self.conn.clone();
        Ok(with_request(server, handle, || async move {
            let (granted, checked) = ask(&conn, &title, &subtitle, &body, &grant, &deny, &label, initial).await;
            let mut results = Results::new();
            if !choices.is_empty() {
                results.insert("choices".into(), value(Value::from(answered_choices(&choices, check.map(|i| (i, checked))))));
            }
            (if granted { response::SUCCESS } else { response::CANCELLED }, results)
        })
        .await)
    }
}

// ----- Account ---------------------------------------------------------

/// From `/etc/passwd` text: the user name and real name (GECOS up to the
/// first comma, else the user name) of `uid`.
pub fn passwd_entry(passwd: &str, uid: u32) -> Option<(String, String)> {
    passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 5 || f[2].parse::<u32>().ok()? != uid {
            return None;
        }
        let real = f[4].split(',').next().unwrap_or("").trim();
        Some((f[0].to_owned(), if real.is_empty() { f[0].to_owned() } else { real.to_owned() }))
    })
}

fn my_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(u32::MAX)
}

/// The user's picture: AccountsService's `IconFile`, else `~/.face`.
async fn picture(uid: u32) -> Option<PathBuf> {
    let from_accounts = async {
        let system = Connection::system().await.ok()?;
        let reply = system.call_method(Some("org.freedesktop.Accounts"), "/org/freedesktop/Accounts", Some("org.freedesktop.Accounts"), "FindUserById", &(i64::from(uid),)).await.ok()?;
        let path: zbus::zvariant::OwnedObjectPath = reply.body().deserialize().ok()?;
        let user = zbus::Proxy::new(&system, "org.freedesktop.Accounts", path, "org.freedesktop.Accounts.User").await.ok()?;
        user.get_property::<String>("IconFile").await.ok().map(PathBuf::from)
    };
    if let Some(p) = from_accounts.await.filter(|p| p.is_file()) {
        return Some(p);
    }
    std::env::var_os("HOME").map(|h| Path::new(&h).join(".face")).filter(|p| p.is_file())
}

pub struct AccountBackend {
    pub conn: Connection,
}

#[interface(name = "org.freedesktop.impl.portal.Account")]
impl AccountBackend {
    async fn get_user_information(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: ObjectPath<'_>,
        app_id: String,
        _window: String,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        let uid = my_uid();
        let (user, real) = passwd_entry(&std::fs::read_to_string("/etc/passwd").unwrap_or_default(), uid)
            .unwrap_or_else(|| (std::env::var("USER").unwrap_or_default(), std::env::var("USER").unwrap_or_default()));
        let reason = opt_str(&options, "reason").map(|r| format!("{r}\n")).unwrap_or_default();
        let message = format!("{} wants to know your name.", app_name(&app_id));
        let description = format!("{reason}Name: {real} ({user})");
        let conn = self.conn.clone();
        Ok(with_request(server, handle, || async move {
            let (granted, _) = ask(&conn, "Share Details", &message, &description, "Share", "Cancel", "", false).await;
            if !granted {
                return (response::CANCELLED, Results::new());
            }
            let mut results = Results::from([("id".to_owned(), value(Value::from(user))), ("name".to_owned(), value(Value::from(real)))]);
            if let Some(p) = picture(uid).await {
                results.insert("image".into(), value(Value::from(gio::File::for_path(p).uri().to_string())));
            }
            (response::SUCCESS, results)
        })
        .await)
    }
}

// ----- AppChooser ------------------------------------------------------

pub struct AppChooserBackend {
    pub conn: Connection,
}

/// "Choose an app to open <file name>." from `uri` or `filename`.
pub fn open_message(uri: Option<&str>, filename: Option<&str>) -> String {
    let name = filename.map(str::to_owned).or_else(|| uri.map(|u| gio::File::for_uri(u).basename().map_or_else(|| u.to_owned(), |b| b.to_string_lossy().into_owned())));
    match name.filter(|n| !n.is_empty()) {
        Some(n) => format!("Choose an app to open {n}."),
        None => "Choose an app.".into(),
    }
}

#[interface(name = "org.freedesktop.impl.portal.AppChooser")]
impl AppChooserBackend {
    async fn choose_application(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: ObjectPath<'_>,
        _app_id: String,
        _parent_window: String,
        choices: Vec<String>,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        let all: HashMap<String, String> = gio::AppInfo::all().into_iter().filter_map(|a| Some((a.id()?.trim_end_matches(".desktop").to_owned(), a.display_name().to_string()))).collect();
        let items: Vec<(String, String)> = choices.iter().filter_map(|c| Some((c.clone(), all.get(c)?.clone()))).collect();
        let last = opt_str(&options, "last_choice").unwrap_or_default();
        let message = open_message(opt_str(&options, "uri").as_deref(), opt_str(&options, "filename").as_deref());
        let token = opt_str(&options, "activation_token");
        let conn = self.conn.clone();
        Ok(with_request(server, handle, || async move {
            if items.is_empty() {
                return (response::CANCELLED, Results::new());
            }
            let reply = conn.call_method(Some(SHELL), DIALOG_PATH, Some(DIALOG), "ChooseApp", &("Open With", message.as_str(), &items, last.as_str())).await;
            let choice: String = reply.and_then(|r| r.body().deserialize()).unwrap_or_default();
            if choice.is_empty() {
                return (response::CANCELLED, Results::new());
            }
            let mut results = Results::from([("choice".to_owned(), value(Value::from(choice)))]);
            if let Some(t) = token {
                results.insert("activation_token".into(), value(Value::from(t)));
            }
            (response::SUCCESS, results)
        })
        .await)
    }

    async fn update_choices(&self, _handle: ObjectPath<'_>, _choices: Vec<String>) {}
}

// ----- Background ------------------------------------------------------

/// A command line as a desktop entry `Exec` value.
pub fn exec_line(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            let escaped = a.replace('%', "%%");
            let reserved = |c: char| c.is_whitespace() || "\"'\\><~|&;$*?#()`".contains(c);
            if escaped.is_empty() || escaped.chars().any(reserved) {
                let mut q = String::from('"');
                for c in escaped.chars() {
                    if matches!(c, '"' | '`' | '$' | '\\') {
                        q.push('\\');
                    }
                    q.push(c);
                }
                q.push('"');
                q
            } else {
                escaped
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The autostart file for `app_id`.
pub fn autostart_entry(app_id: &str, commandline: &[String], dbus: bool) -> String {
    let mut s = format!("[Desktop Entry]\nType=Application\nName={app_id}\nExec={}\nX-Flatpak={app_id}\n", exec_line(commandline));
    if dbus {
        s.push_str("DBusActivatable=true\n");
    }
    s
}

fn autostart_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config"))).map(|c| c.join("autostart"))
}

pub struct BackgroundBackend {
    pub conn: Connection,
}

#[interface(name = "org.freedesktop.impl.portal.Background")]
impl BackgroundBackend {
    async fn get_app_state(&self) -> fdo::Result<HashMap<String, OwnedValue>> {
        let reply = self.conn.call_method(Some(SHELL), DIALOG_PATH, Some(DIALOG), "RunningApps", &()).await;
        let apps: HashMap<String, u32> = reply.and_then(|r| r.body().deserialize()).unwrap_or_default();
        Ok(apps.into_iter().map(|(k, v)| (k, value(Value::from(v)))).collect())
    }

    async fn notify_background(&self, #[zbus(object_server)] server: &ObjectServer, handle: ObjectPath<'_>, app_id: String, name: String) -> fdo::Result<(u32, Results)> {
        let name = if name.is_empty() { app_name(&app_id) } else { name };
        let conn = self.conn.clone();
        Ok(with_request(server, handle, || async move {
            let (allow, _) = ask(&conn, "Background Activity", &format!("{name} is running in the background."), "", "Allow", "Forbid", "", false).await;
            (response::SUCCESS, Results::from([("result".to_owned(), value(Value::from(u32::from(allow))))]))
        })
        .await)
    }

    async fn enable_autostart(&self, app_id: String, enable: bool, commandline: Vec<String>, flags: u32) -> fdo::Result<bool> {
        if app_id.is_empty() || app_id.contains('/') {
            return Ok(false);
        }
        let Some(dir) = autostart_dir() else { return Ok(false) };
        let file = dir.join(format!("{app_id}.desktop"));
        let done = if enable {
            std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&file, autostart_entry(&app_id, &commandline, flags & 1 != 0)))
        } else {
            match std::fs::remove_file(&file) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            }
        };
        tracing::info!(app_id, enable, ok = done.is_ok(), "autostart");
        Ok(done.is_ok())
    }

    #[zbus(signal)]
    async fn running_applications_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Pass the shell's `RunningAppsChanged` on as `RunningApplicationsChanged`.
pub async fn follow_running(conn: Connection, path: &'static str) {
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(DIALOG).and_then(|b| b.member("RunningAppsChanged")).map(|b| b.build());
    let Ok(rule) = rule else { return };
    let Ok(mut stream) = zbus::MessageStream::for_match_rule(rule, &conn, Some(16)).await else { return };
    while stream.next().await.is_some() {
        if let Ok(emitter) = SignalEmitter::new(&conn, path) {
            let _ = BackgroundBackend::running_applications_changed(&emitter).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_choices() {
        let c = |id: &str, opts: &[(&str, &str)], init: &str| -> Choice { (id.into(), id.to_uppercase(), opts.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(), init.into()) };
        let choices = vec![c("quality", &[("hi", "High"), ("lo", "Low")], "hi"), c("remember", &[], "false"), c("other", &[], "true")];
        assert_eq!(check_box(&choices), Some(1));
        let answered = answered_choices(&choices, Some((1, true)));
        assert_eq!(answered, [("quality".into(), "hi".into()), ("remember".into(), "true".into()), ("other".into(), "true".into())]);
    }

    #[test]
    fn accounts() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\nada:x:1000:1000:Ada Lovelace,,,:/home/ada:/bin/bash\nbob:x:1001:1001::/home/bob:/bin/sh\n";
        assert_eq!(passwd_entry(passwd, 1000), Some(("ada".into(), "Ada Lovelace".into())));
        assert_eq!(passwd_entry(passwd, 1001), Some(("bob".into(), "bob".into())));
        assert_eq!(passwd_entry(passwd, 7), None);
    }

    #[test]
    fn autostart_files() {
        let args: Vec<String> = ["flatpak", "run", "--command=my app", "org.example.App", "100%"].iter().map(|s| s.to_string()).collect();
        assert_eq!(exec_line(&args), "flatpak run \"--command=my app\" org.example.App 100%%");
        assert_eq!(exec_line(&["say".into(), "a$b".into()]), "say \"a\\$b\"");
        let entry = autostart_entry("org.example.App", &args[..2], true);
        assert!(entry.contains("Exec=flatpak run\n") && entry.contains("X-Flatpak=org.example.App") && entry.ends_with("DBusActivatable=true\n"));
        assert_eq!(open_message(Some("file:///tmp/My%20Doc.pdf"), None), "Choose an app to open My Doc.pdf.");
        assert_eq!(open_message(None, None), "Choose an app.");
    }
}
