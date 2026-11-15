//! Apps the shell starts each get a systemd scope of their own,
//! `app-helix-<app id>-<pid>.scope` in the user manager's `app.slice`
//! (the naming systemd's desktop-environment conventions give, as GNOME
//! does for apps it launches). systemd-oomd and resource controls then
//! see each app apart from the session's core processes, and an app that
//! runs away under memory pressure is a clear candidate to stop.
//!
//! Launches go through a [`gio::AppLaunchContext`] from [`context`]: GIO
//! reports each spawned process (`launched`, with its pid), and the pid
//! moves into a new transient scope by `StartTransientUnit` on the user
//! manager (`org.freedesktop.systemd1` on the session bus). Apps started
//! by D-Bus activation have no pid here; the bus starts them. Without a
//! user manager on the bus the call fails and the app stays where it was
//! started (logged at debug level).

use gio::prelude::*;
use zbus::zvariant::Value;

/// What the scopes say they are.
const DESCRIPTION: &str = "Application launched by helix-shell";

/// `id` escaped for a unit name the way systemd escapes strings: ASCII
/// letters, digits, `:`, `_` and `.` (not leading) stay; every other byte
/// becomes `\xNN` (so `-`, which separates the name's parts, is `\x2d`).
pub fn escape(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for (i, b) in id.bytes().enumerate() {
        let plain = b.is_ascii_alphanumeric() || b == b':' || b == b'_' || (b == b'.' && i > 0);
        if plain {
            out.push(b as char);
        } else {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out
}

/// The scope for app `desktop_id` (with or without `.desktop`) running as
/// `pid`.
pub fn unit_name(desktop_id: &str, pid: u32) -> String {
    let id = desktop_id.strip_suffix(".desktop").unwrap_or(desktop_id);
    format!("app-helix-{}-{pid}.scope", escape(id))
}

/// Move `pid` into a new transient scope for `desktop_id` through the
/// user manager reachable on `conn`; returns the job's path.
pub async fn place_on(conn: &zbus::Connection, desktop_id: &str, pid: u32) -> zbus::Result<zbus::zvariant::OwnedObjectPath> {
    let unit = unit_name(desktop_id, pid);
    let properties: Vec<(&str, Value<'_>)> = vec![
        ("Description", Value::from(DESCRIPTION)),
        ("PIDs", Value::from(vec![pid])),
        ("Slice", Value::from("app.slice")),
        // Gone from the manager once it ends, failed or not.
        ("CollectMode", Value::from("inactive-or-failed")),
    ];
    let aux: Vec<(&str, Vec<(&str, Value<'_>)>)> = Vec::new();
    let reply = conn
        .call_method(Some("org.freedesktop.systemd1"), "/org/freedesktop/systemd1", Some("org.freedesktop.systemd1.Manager"), "StartTransientUnit", &(unit.as_str(), "fail", properties, aux))
        .await?;
    reply.body().deserialize()
}

/// Place `pid` on a worker thread, on the session bus (whose user manager
/// owns `org.freedesktop.systemd1`); the launch doesn't wait for it.
fn place(desktop_id: String, pid: u32) {
    let spawned = std::thread::Builder::new().name("app-scope".into()).spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        rt.block_on(async {
            let placed = async {
                let conn = zbus::Connection::session().await?;
                place_on(&conn, &desktop_id, pid).await
            };
            match placed.await {
                Ok(_) => tracing::debug!(app = desktop_id, pid, "app in its own scope"),
                Err(err) => tracing::debug!(%err, app = desktop_id, pid, "app not moved to a scope of its own"),
            }
        });
    });
    if let Err(err) = spawned {
        tracing::debug!(%err, "cannot start the app scope thread");
    }
}

/// A launch context that puts each process it launches in a scope of its
/// own. `desktop_id` names the app when the launched [`gio::AppInfo`] has
/// no id of its own (one made from a desktop action's command line).
pub fn context(desktop_id: Option<&str>) -> gio::AppLaunchContext {
    context_with(desktop_id, place)
}

/// [`context`] reporting each launched (app id, pid) to `placed` instead.
pub fn context_with(desktop_id: Option<&str>, placed: impl Fn(String, u32) + 'static) -> gio::AppLaunchContext {
    let ctx = gio::AppLaunchContext::new();
    let fallback = desktop_id.map(str::to_owned);
    ctx.connect_launched(move |_, info, platform_data| {
        let data = platform_data.get::<std::collections::HashMap<String, gio::glib::Variant>>().unwrap_or_default();
        let pid = data.get("pid").and_then(|v| v.get::<i32>()).filter(|p| *p > 0);
        let id = info.id().map(|i| i.to_string()).or_else(|| fallback.clone());
        if let (Some(pid), Some(id)) = (pid, id) {
            placed(id, pid as u32);
        }
    });
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn unit_names() {
        assert_eq!(unit_name("org.gnome.Calculator.desktop", 42), "app-helix-org.gnome.Calculator-42.scope");
        assert_eq!(unit_name("google-chrome.desktop", 7), "app-helix-google\\x2dchrome-7.scope");
        assert_eq!(unit_name("firefox_firefox", 7), "app-helix-firefox_firefox-7.scope");
        assert_eq!(escape(".hidden app/x"), "\\x2ehidden\\x20app\\x2fx");
    }

    /// What the fake user manager was asked.
    type Asked = Arc<Mutex<Vec<(String, String, Vec<(String, zbus::zvariant::OwnedValue)>)>>>;

    struct FakeManager {
        asked: Asked,
    }

    #[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
    impl FakeManager {
        async fn start_transient_unit(
            &self,
            name: String,
            mode: String,
            properties: Vec<(String, zbus::zvariant::OwnedValue)>,
            _aux: Vec<(String, Vec<(String, zbus::zvariant::OwnedValue)>)>,
        ) -> zbus::zvariant::OwnedObjectPath {
            self.asked.lock().unwrap().push((name, mode, properties));
            zbus::zvariant::OwnedObjectPath::try_from("/org/freedesktop/systemd1/job/1").unwrap()
        }
    }

    /// Against a fake user manager on a private peer-to-peer connection
    /// (never the real one): the pid goes into its own scope in
    /// `app.slice`, collected when it ends.
    #[test]
    fn asks_the_user_manager_for_a_scope() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let (a, b) = tokio::net::UnixStream::pair().unwrap();
            let asked = Asked::default();
            let fake = FakeManager { asked: asked.clone() };
            let server = zbus::connection::Builder::unix_stream(a).server(zbus::Guid::generate()).unwrap().p2p().serve_at("/org/freedesktop/systemd1", fake).unwrap().build();
            let client = zbus::connection::Builder::unix_stream(b).p2p().build();
            let (server, client) = tokio::join!(server, client);
            let (_server, client) = (server.unwrap(), client.unwrap());
            let job = place_on(&client, "org.example.App.desktop", 4242).await.unwrap();
            assert_eq!(job.as_str(), "/org/freedesktop/systemd1/job/1");
            let asked = asked.lock().unwrap();
            let (name, mode, properties) = &asked[0];
            assert_eq!(name, "app-helix-org.example.App-4242.scope");
            assert_eq!(mode, "fail");
            let get = |key: &str| properties.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()).unwrap_or_else(|| panic!("{key}"));
            assert_eq!(Vec::<u32>::try_from(get("PIDs")).unwrap(), [4242]);
            assert_eq!(String::try_from(get("Slice")).unwrap(), "app.slice");
            assert_eq!(String::try_from(get("CollectMode")).unwrap(), "inactive-or-failed");
            assert_eq!(String::try_from(get("Description")).unwrap(), DESCRIPTION);
        });
    }

    /// A launch through the context reports the app's id and the pid of
    /// the process GIO spawned.
    #[test]
    fn launches_report_their_pid() {
        let dir = std::env::temp_dir().join(format!("helix-app-scope-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("pid");
        let command = format!("sh -c 'echo $$ > {}'", marker.display());
        let info = gio::AppInfo::create_from_commandline(command, Some("Probe"), gio::AppInfoCreateFlags::NONE).unwrap();
        let placed: Arc<Mutex<Vec<(String, u32)>>> = Arc::default();
        let seen = placed.clone();
        let ctx = context_with(Some("org.example.Probe.desktop"), move |id, pid| seen.lock().unwrap().push((id, pid)));
        info.launch(&[], Some(&ctx)).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let written = loop {
            if let Some(pid) = std::fs::read_to_string(&marker).ok().and_then(|s| s.trim().parse::<u32>().ok()) {
                break pid;
            }
            assert!(std::time::Instant::now() < deadline, "the probe did not run");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let placed = placed.lock().unwrap();
        assert_eq!(placed.len(), 1, "{placed:?}");
        assert_eq!(placed[0].0, "org.example.Probe.desktop");
        assert_eq!(placed[0].1, written, "the app's own pid");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
