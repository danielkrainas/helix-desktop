//! What the shell asks of systemd-logind (helix-desktop-spec
//! `org.freedesktop.login1.md`): which power actions the Power menu may
//! offer, and locking before the system sleeps
//! (`org.gnome.ScreenSaver.md`, "Locking before suspend").

use std::future::Future;

use futures_util::StreamExt;
use zbus::zvariant::OwnedFd;
use zbus::{Connection, MatchRule, MessageStream};

const BUS_NAME: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";

/// Power actions the system allows without asking for a password. The
/// session manager calls logind non-interactively, so `challenge` (needs
/// authentication) counts as unavailable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Availability {
    pub suspend: bool,
    pub restart: bool,
    pub power_off: bool,
}

async fn allowed(conn: &Connection, method: &str) -> bool {
    let reply = conn
        .call_method(Some(BUS_NAME), MANAGER_PATH, Some(MANAGER_IFACE), method, &())
        .await;
    match reply.and_then(|r| r.body().deserialize::<String>()) {
        Ok(answer) => answer == "yes",
        Err(err) => {
            tracing::debug!(%err, method, "cannot ask logind");
            false
        }
    }
}

pub async fn availability(conn: &Connection) -> Availability {
    Availability {
        suspend: allowed(conn, "CanSuspend").await,
        restart: allowed(conn, "CanReboot").await,
        power_off: allowed(conn, "CanPowerOff").await,
    }
}

async fn take_delay(conn: &Connection) -> Option<OwnedFd> {
    let reply = conn
        .call_method(
            Some(BUS_NAME),
            MANAGER_PATH,
            Some(MANAGER_IFACE),
            "Inhibit",
            &("sleep", "Helix", "Lock the screen before suspending", "delay"),
        )
        .await;
    match reply.and_then(|r| r.body().deserialize::<OwnedFd>()) {
        Ok(fd) => Some(fd),
        Err(err) => {
            tracing::warn!(%err, "cannot delay sleep; the screen may not lock before suspend");
            None
        }
    }
}

/// Run until logind goes away: hold a sleep delay inhibitor, and when the
/// system is about to sleep, run `lock` (lock if the settings ask for it,
/// and wait until the lock screen is shown), then let it sleep.
pub async fn lock_before_sleep<L, Fut>(conn: Connection, mut lock: L)
where
    L: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(BUS_NAME)
        .and_then(|b| b.interface(MANAGER_IFACE))
        .and_then(|b| b.member("PrepareForSleep"))
        .map(|b| b.build());
    let stream = match rule {
        Ok(rule) => MessageStream::for_match_rule(rule, &conn, None).await,
        Err(err) => Err(err),
    };
    let mut signals = match stream {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(%err, "cannot follow logind sleep; the screen will not lock before suspend");
            return;
        }
    };
    let mut held = take_delay(&conn).await;
    while let Some(Ok(message)) = signals.next().await {
        let Ok(start) = message.body().deserialize::<bool>() else { continue };
        if start {
            tracing::info!("system is about to sleep");
            lock().await;
            held = None;
        } else {
            tracing::info!("system resumed");
            if held.is_none() {
                held = take_delay(&conn).await;
            }
        }
    }
}
