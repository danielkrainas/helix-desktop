//! The people who can log in (helixdesktop/spec `components/greeter.md`,
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
    /// Their picture (AccountsService `IconFile`), when it exists.
    pub icon: Option<std::path::PathBuf>,
    /// The session they last used, as AccountsService remembers it
    /// (`Session`, else `XSession`): a session id.
    pub session: Option<String>,
}

impl User {
    pub fn new(name: &str, real_name: &str, login_time: i64) -> Self {
        let real_name = real_name.trim();
        let display = if real_name.is_empty() { name } else { real_name };
        Self { name: name.to_owned(), display: display.to_owned(), login_time, icon: None, session: None }
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
    let mut user = User::new(&name, &string("RealName"), login_time);
    let icon = string("IconFile");
    user.icon = Some(std::path::PathBuf::from(icon)).filter(|p| p.is_absolute() && p.is_file());
    let session = [string("Session"), string("XSession")].into_iter().find(|s| !s.trim().is_empty());
    user.session = session.map(|s| s.trim().trim_end_matches(".desktop").to_owned());
    Some(user)
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
        assert_eq!(users, vec![User::new("ann", "Ann Smith", 0), User::new("bob", "", 0), User::new("noshell", "", 0)]);
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
        assert_eq!(user, Some(User::new("ann", "Ann Smith", 1_700_000_000)));
        let user = user_from_properties(&props(vec![("UserName", "bob".into()), ("RealName", "".into())]));
        assert_eq!(user.unwrap().display, "bob");
        assert_eq!(user_from_properties(&props(vec![("UserName", "gdm".into()), ("SystemAccount", true.into())])), None);
    }

    /// A fake AccountsService user (`org.freedesktop.Accounts.User`).
    struct FakeUser {
        name: String,
        real: String,
        icon: String,
        session: String,
        x_session: String,
        system: bool,
        login_time: i64,
    }

    #[zbus::interface(name = "org.freedesktop.Accounts.User")]
    impl FakeUser {
        #[zbus(property)]
        fn user_name(&self) -> String {
            self.name.clone()
        }
        #[zbus(property)]
        fn real_name(&self) -> String {
            self.real.clone()
        }
        #[zbus(property)]
        fn icon_file(&self) -> String {
            self.icon.clone()
        }
        #[zbus(property)]
        fn session(&self) -> String {
            self.session.clone()
        }
        #[zbus(property, name = "XSession")]
        fn x_session(&self) -> String {
            self.x_session.clone()
        }
        #[zbus(property)]
        fn system_account(&self) -> bool {
            self.system
        }
        #[zbus(property)]
        fn login_time(&self) -> i64 {
            self.login_time
        }
    }

    struct FakeAccounts {
        paths: Vec<OwnedObjectPath>,
    }

    #[zbus::interface(name = "org.freedesktop.Accounts")]
    impl FakeAccounts {
        fn list_cached_users(&self) -> Vec<OwnedObjectPath> {
            self.paths.clone()
        }
    }

    /// Against a fake AccountsService on a private bus standing in for the
    /// system bus (never the real one): names, pictures that exist,
    /// remembered sessions (`Session`, else `XSession`), system accounts
    /// left out.
    #[test]
    fn fake_accounts_service() {
        let Some(bus) = helix_test_support::TestBus::start() else { return };
        let dir = std::env::temp_dir().join(format!("helix-greeter-icons-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let icon = dir.join("ann");
        std::fs::write(&icon, b"\x89PNG").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let user = |name: &str, real: &str, icon: &str, session: &str, x_session: &str, system: bool, login_time: i64| FakeUser {
                name: name.into(),
                real: real.into(),
                icon: icon.into(),
                session: session.into(),
                x_session: x_session.into(),
                system,
                login_time,
            };
            let users = [
                ("/org/freedesktop/Accounts/User1000", user("ann", "Ann Smith", icon.to_str().unwrap(), "helix", "", false, 20)),
                ("/org/freedesktop/Accounts/User1001", user("bob", "", "/nonexistent/bob", "", "sway.desktop", false, 10)),
                ("/org/freedesktop/Accounts/User120", user("gdm", "Gnome Display Manager", "", "", "", true, 0)),
            ];
            let paths: Vec<OwnedObjectPath> = users.iter().map(|(p, _)| OwnedObjectPath::try_from(*p).unwrap()).collect();
            let mut builder = bus.builder().name(ACCOUNTS).unwrap().serve_at(ACCOUNTS_PATH, FakeAccounts { paths }).unwrap();
            for (path, user) in users {
                builder = builder.serve_at(path, user).unwrap();
            }
            let _service = builder.build().await.unwrap();
            let system = bus.connect().await;
            let users = from_accounts_service(&system).await.expect("AccountsService answers");
            let mut ann = User::new("ann", "Ann Smith", 20);
            ann.icon = Some(icon.clone());
            ann.session = Some("helix".into());
            let mut bob = User::new("bob", "", 10);
            bob.session = Some("sway".into());
            assert_eq!(users, vec![ann, bob], "the picture that exists, sessions, no system account");
        });
        let _ = std::fs::remove_dir_all(&dir);
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
