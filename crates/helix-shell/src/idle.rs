//! Lock the screen when the user has been idle, per GNOME's settings:
//! after `org.gnome.desktop.session idle-delay` + `org.gnome.desktop.screensaver
//! lock-delay` seconds, if `lock-enabled`, unless an idle inhibitor is active.

use std::time::Duration;

use futures_util::StreamExt;
use gio::prelude::*;
use zbus::Connection;

const SESSION: &str = "org.gnome.desktop.session";
const SCREENSAVER: &str = "org.gnome.desktop.screensaver";
/// SessionManager inhibit flag for "the session being marked idle".
const INHIBIT_IDLE: u32 = 8;

#[zbus::proxy(
    interface = "org.gnome.Mutter.IdleMonitor",
    default_service = "org.gnome.Mutter.IdleMonitor",
    default_path = "/org/gnome/Mutter/IdleMonitor/Core"
)]
trait IdleMonitor {
    fn add_idle_watch(&self, interval: u64) -> zbus::Result<u32>;
    fn remove_watch(&self, id: u32) -> zbus::Result<()>;
    #[zbus(signal)]
    fn watch_fired(&self, id: u32) -> zbus::Result<()>;
}

/// How long the user must be idle before locking, if at all.
pub fn lock_timeout(idle_delay: u32, lock_enabled: bool, lock_delay: u32) -> Option<Duration> {
    (lock_enabled && idle_delay > 0)
        .then(|| Duration::from_secs(u64::from(idle_delay) + u64::from(lock_delay)))
}

fn read_timeout() -> Option<Duration> {
    if !helix_settings::has_schema(SESSION) || !helix_settings::has_schema(SCREENSAVER) {
        return None;
    }
    let session = gio::Settings::new(SESSION);
    let screensaver = gio::Settings::new(SCREENSAVER);
    lock_timeout(
        session.uint("idle-delay"),
        screensaver.boolean("lock-enabled"),
        screensaver.uint("lock-delay"),
    )
}

async fn idle_inhibited(conn: &Connection) -> bool {
    conn.call_method(
        Some("org.gnome.SessionManager"),
        "/org/gnome/SessionManager",
        Some("org.gnome.SessionManager"),
        "IsInhibited",
        &(INHIBIT_IDLE,),
    )
    .await
    .ok()
    .and_then(|reply| reply.body().deserialize::<bool>().ok())
    .unwrap_or(false)
}

/// Run forever: keep an idle watch matching the settings, lock when it fires.
pub async fn run(conn: Connection) {
    let settings = helix_settings::GSettings::new(&[SESSION, SCREENSAVER]);
    let (mut changes, _watch) = settings.watch();
    let monitor = match IdleMonitorProxy::new(&conn).await {
        Ok(m) => m,
        Err(err) => {
            tracing::warn!(%err, "no idle monitor; idle locking disabled");
            return;
        }
    };
    let Ok(mut fired) = monitor.receive_watch_fired().await else {
        tracing::warn!("cannot watch the idle monitor; idle locking disabled");
        return;
    };
    let mut watch: Option<u32> = None;
    let mut rearm = true;
    loop {
        if rearm {
            rearm = false;
            if let Some(id) = watch.take() {
                let _ = monitor.remove_watch(id).await;
            }
            match read_timeout() {
                Some(timeout) => match monitor.add_idle_watch(timeout.as_millis() as u64).await {
                    Ok(id) => {
                        tracing::info!(?timeout, "idle lock armed");
                        watch = Some(id);
                    }
                    Err(err) => {
                        tracing::debug!(%err, "idle monitor not ready; retrying");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        rearm = true;
                        continue;
                    }
                },
                None => tracing::info!("idle locking disabled by settings"),
            }
        }
        tokio::select! {
            Some(signal) = fired.next() => {
                let Ok(args) = signal.args() else { continue };
                if Some(args.id) != watch {
                    continue;
                }
                if idle_inhibited(&conn).await {
                    tracing::info!("idle, but an idle inhibitor is active; not locking");
                } else if let Err(err) = crate::lock_via(&conn).await {
                    tracing::warn!(%err, "idle lock failed");
                }
            }
            Some(_) = changes.recv() => rearm = true,
            else => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_from_settings() {
        assert_eq!(lock_timeout(300, true, 0), Some(Duration::from_secs(300)));
        assert_eq!(lock_timeout(300, true, 30), Some(Duration::from_secs(330)));
        assert_eq!(lock_timeout(0, true, 30), None, "idle-delay 0 means never");
        assert_eq!(lock_timeout(300, false, 0), None, "lock disabled");
    }
}
