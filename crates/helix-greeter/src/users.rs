//! The people who can log in (helix-desktop-spec `components/greeter.md`,
//! "User list"): AccountsService's cached users that are not system
//! accounts, else the ordinary accounts in `/etc/passwd`.

use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, OwnedValue};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// The login name greetd takes.
    pub name: String,
    /// What the list shows: the real name, else the login name.
    pub display: String,
    /// Last login, seconds since the epoch (0: never or unknown).
    pub login_time: i64,
}

impl User {
    fn new(name: &str, real_name: &str, login_time: i64) -> Self {
        let real_name = real_name.trim();
        let display = if real_name.is_empty() { name } else { real_name };
        Self { name: name.to_owned(), display: display.to_owned(), login_time }
    }
}

/// Most recently logged in first, then by display name.
pub fn sort(users: &mut [User]) {
    users.sort_by(|a, b| b.login_time.cmp(&a.login_time).then_with(|| a.display.to_lowercase().cmp(&b.display.to_lowercase())).then_with(|| a.name.cmp(&b.name)));
}

/// Accounts in `/etc/passwd` people log in with: a UID from 1000 to 59999
/// and a login shell.
pub fn parse_passwd(contents: &str) -> Vec<User> {
    let mut users = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split(':').collect();
        let [name, _password, uid, _gid, gecos, _home, shell] = fields[..] else {
            continue;
        };
        let Ok(uid) = uid.parse::<u32>() else {
            continue;
        };
        if !(1000..=59999).contains(&uid) || !login_shell(shell) || name.is_empty() {
            continue;
        }
        // The GECOS field's first part is the full name.
        let real_name = gecos.split(',').next().unwrap_or_default();
        users.push(User::new(name, real_name, 0));
    }
    users
}

/// An empty shell field means `/bin/sh`.
fn login_shell(shell: &str) -> bool {
    let program = shell.rsplit('/').next().unwrap_or_default();
    program != "nologin" && program != "false"
}

/// The users from `/etc/passwd`, sorted.
pub fn from_passwd() -> Vec<User> {
    let mut users = std::fs::read_to_string("/etc/passwd").map(|c| parse_passwd(&c)).unwrap_or_default();
    sort(&mut users);
    users
}

const ACCOUNTS: &str = "org.freedesktop.Accounts";
const ACCOUNTS_PATH: &str = "/org/freedesktop/Accounts";
const USER_IFACE: &str = "org.freedesktop.Accounts.User";

/// The users AccountsService knows, sorted; `None` without AccountsService.
pub async fn from_accounts_service(system: &zbus::Connection) -> Option<Vec<User>> {
    let reply = system.call_method(Some(ACCOUNTS), ACCOUNTS_PATH, Some(ACCOUNTS), "ListCachedUsers", &()).await;
    let paths: Vec<OwnedObjectPath> = match reply.and_then(|r| r.body().deserialize::<Vec<OwnedObjectPath>>()) {
        Ok(paths) => paths,
        Err(err) => {
            tracing::info!(%err, "no AccountsService; reading /etc/passwd");
            return None;
        }
    };
    let mut users = Vec::new();
    for path in paths {
        let reply = system.call_method(Some(ACCOUNTS), path.as_str(), Some("org.freedesktop.DBus.Properties"), "GetAll", &(USER_IFACE,)).await;
        let props: HashMap<String, OwnedValue> = match reply.and_then(|r| r.body().deserialize()) {
            Ok(props) => props,
            Err(err) => {
                tracing::warn!(%err, path = path.as_str(), "cannot read AccountsService user");
                continue;
            }
        };
        if let Some(user) = user_from_properties(&props) {
            users.push(user);
        }
    }
    sort(&mut users);
    Some(users)
}

fn user_from_properties(props: &HashMap<String, OwnedValue>) -> Option<User> {
    let string = |key: &str| props.get(key).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
    let flag = |key: &str| props.get(key).and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
    let name = string("UserName");
    if name.is_empty() || flag("SystemAccount") {
        return None;
    }
    let login_time = props.get("LoginTime").and_then(|v| i64::try_from(v).ok()).unwrap_or(0);
    Some(User::new(&name, &string("RealName"), login_time))
}

/// The users to list: AccountsService's, else `/etc/passwd`'s.
pub async fn load(system: Option<&zbus::Connection>) -> Vec<User> {
    if let Some(system) = system {
        if let Some(users) = from_accounts_service(system).await {
            return users;
        }
    }
    from_passwd()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    #[test]
    fn passwd() {
        let passwd = "\
root:x:0:0:root:/root:/bin/bash
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
# a comment

ann:x:1000:1000:Ann Smith,,,:/home/ann:/bin/bash
_greetd:x:998:998::/var/lib/greetd:/usr/sbin/nologin
svc:x:1001:1001:Service:/srv:/usr/sbin/nologin
bob:x:1002:1002::/home/bob:/usr/bin/zsh
nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin
ftp:x:1003:1003::/srv/ftp:/bin/false
big:x:60000:60000::/home/big:/bin/bash
broken:x:notanumber:1::/:/bin/sh
short:x:1004
noshell:x:1005:1005:::
";
        let users = parse_passwd(passwd);
        assert_eq!(users, vec![User { name: "ann".into(), display: "Ann Smith".into(), login_time: 0 }, User { name: "bob".into(), display: "bob".into(), login_time: 0 }, User { name: "noshell".into(), display: "noshell".into(), login_time: 0 }]);
    }

    #[test]
    fn sorted_by_last_login() {
        let mut users = vec![User::new("a", "Zed", 10), User::new("b", "", 0), User::new("c", "Amy", 10), User::new("d", "Dee", 99)];
        sort(&mut users);
        let names: Vec<&str> = users.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, ["d", "c", "a", "b"]);
    }

    #[test]
    fn accounts_service_properties() {
        let props = |pairs: Vec<(&str, Value)>| -> HashMap<String, OwnedValue> { pairs.into_iter().map(|(k, v)| (k.to_owned(), v.try_to_owned().unwrap())).collect() };
        let user = user_from_properties(&props(vec![("UserName", "ann".into()), ("RealName", "Ann Smith".into()), ("LoginTime", 1_700_000_000i64.into()), ("SystemAccount", false.into())]));
        assert_eq!(user, Some(User { name: "ann".into(), display: "Ann Smith".into(), login_time: 1_700_000_000 }));
        let user = user_from_properties(&props(vec![("UserName", "bob".into()), ("RealName", "".into())]));
        assert_eq!(user.unwrap().display, "bob");
        assert_eq!(user_from_properties(&props(vec![("UserName", "gdm".into()), ("SystemAccount", true.into())])), None);
    }

    /// Read-only, against the host's system bus; skipped without one.
    #[test]
    fn host_accounts_service() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let Ok(system) = zbus::Connection::system().await else {
                eprintln!("no system bus; skipping");
                return;
            };
            if let Some(users) = from_accounts_service(&system).await {
                assert!(users.windows(2).all(|w| w[0].login_time >= w[1].login_time));
                assert!(users.iter().all(|u| !u.name.is_empty() && !u.display.is_empty()));
            }
        });
    }
}
