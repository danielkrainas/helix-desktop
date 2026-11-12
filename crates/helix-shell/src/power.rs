//! Power management (helix-desktop-spec `contracts/gsettings/power.md`):
//! dimming, blanking, automatic suspend, the power button, the lid while
//! docked, and battery warnings.

use std::time::Duration;

use futures_util::StreamExt;
use gio::prelude::*;
use zbus::zvariant::OwnedFd;
use zbus::Connection;

pub const POWER: &str = "org.gnome.settings-daemon.plugins.power";
const SESSION: &str = "org.gnome.desktop.session";
const SM: &str = "org.gnome.SessionManager";
const SM_PATH: &str = "/org/gnome/SessionManager";
const LOGIND: &str = "org.freedesktop.login1";
const LOGIND_PATH: &str = "/org/freedesktop/login1";
const LOGIND_MANAGER: &str = "org.freedesktop.login1.Manager";
const DISPLAY_CONFIG: &str = "org.gnome.Mutter.DisplayConfig";
const DISPLAY_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
const UPOWER: &str = "org.freedesktop.UPower";
const INHIBIT_SUSPEND: u32 = 4;
const INHIBIT_IDLE: u32 = 8;

/// `sleep-inactive-*-type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleAction {
    Blank,
    Suspend,
    Shutdown,
    Hibernate,
    Interactive,
    Nothing,
    Logout,
}

impl IdleAction {
    pub fn parse(s: &str) -> Self {
        match s {
            "blank" => IdleAction::Blank,
            "shutdown" => IdleAction::Shutdown,
            "hibernate" => IdleAction::Hibernate,
            "interactive" => IdleAction::Interactive,
            "nothing" => IdleAction::Nothing,
            "logout" => IdleAction::Logout,
            _ => IdleAction::Suspend,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// `idle-delay` seconds (0: never blank).
    pub idle_delay: u32,
    pub dim: bool,
    pub idle_brightness: u32,
    pub ac: (u32, IdleAction),
    pub battery: (u32, IdleAction),
    pub button: IdleAction,
    pub saver_on_low: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { idle_delay: 300, dim: true, idle_brightness: 30, ac: (900, IdleAction::Suspend), battery: (900, IdleAction::Suspend), button: IdleAction::Suspend, saver_on_low: true }
    }
}

fn settings(schema: &str) -> Option<gio::Settings> {
    gio::SettingsSchemaSource::default()?.lookup(schema, true).map(|_| gio::Settings::new(schema))
}

pub fn read() -> Settings {
    let d = Settings::default();
    let idle_delay = settings(SESSION).map_or(d.idle_delay, |s| s.uint("idle-delay"));
    let Some(p) = settings(POWER) else { return Settings { idle_delay, ..d } };
    let secs = |k: &str| p.int(k).max(0) as u32;
    let button = match p.string("power-button-action").as_str() {
        "nothing" => IdleAction::Nothing,
        "hibernate" => IdleAction::Hibernate,
        "interactive" => IdleAction::Interactive,
        _ => IdleAction::Suspend,
    };
    Settings {
        idle_delay,
        dim: p.boolean("idle-dim"),
        idle_brightness: p.int("idle-brightness").clamp(0, 100) as u32,
        ac: (secs("sleep-inactive-ac-timeout"), IdleAction::parse(&p.string("sleep-inactive-ac-type"))),
        battery: (secs("sleep-inactive-battery-timeout"), IdleAction::parse(&p.string("sleep-inactive-battery-type"))),
        button,
        saver_on_low: p.boolean("power-saver-profile-on-low-battery"),
    }
}

/// When to dim: four fifths of `idle-delay`, at least 10 s before blanking.
pub fn dim_after(idle_delay: u32) -> Option<u32> {
    (idle_delay > 0).then(|| (idle_delay * 4 / 5).min(idle_delay.saturating_sub(10)).max(1))
}

/// The dimmed brightness, if dimming changes anything.
pub fn dim_target(current: u32, idle_brightness: u32) -> Option<u32> {
    (current > idle_brightness).then_some(idle_brightness)
}

async fn inhibited(conn: &Connection, flag: u32) -> bool {
    conn.call_method(Some(SM), SM_PATH, Some(SM), "IsInhibited", &(flag,)).await.ok().and_then(|r| r.body().deserialize::<bool>().ok()).unwrap_or(false)
}

async fn sm(conn: &Connection, member: &str, mode: Option<u32>) {
    let result = match mode {
        Some(m) => conn.call_method(Some(SM), SM_PATH, Some(SM), member, &(m,)).await,
        None => conn.call_method(Some(SM), SM_PATH, Some(SM), member, &()).await,
    };
    if let Err(err) = result {
        tracing::warn!(%err, member, "session manager refused");
    }
}

/// Whether the session manager offers `member` (`CanSuspend`, …) at all:
/// 0 means unavailable (no logind, or a session that may not power off).
async fn session_may(conn: &Connection, member: &str) -> bool {
    conn.call_method(Some(SM), SM_PATH, Some(SM), member, &()).await.ok().and_then(|r| r.body().deserialize::<u32>().ok()).unwrap_or(0) != 0
}

/// Hibernate through logind, when the system can and the session may
/// suspend at all.
pub async fn hibernate(session: &Connection) {
    if !session_may(session, "CanSuspend").await {
        tracing::info!("this session may not suspend; not hibernating");
        return;
    }
    let Ok(system) = Connection::system().await else { return };
    let can: String = match system.call_method(Some(LOGIND), LOGIND_PATH, Some(LOGIND_MANAGER), "CanHibernate", &()).await {
        Ok(r) => r.body().deserialize().unwrap_or_default(),
        Err(_) => return,
    };
    if can != "yes" {
        tracing::info!(can, "cannot hibernate");
        return;
    }
    if let Err(err) = system.call_method(Some(LOGIND), LOGIND_PATH, Some(LOGIND_MANAGER), "Hibernate", &(false,)).await {
        tracing::warn!(%err, "hibernate failed");
    }
}

/// Do an idle or power button action.
pub async fn act(conn: &Connection, action: IdleAction) {
    tracing::info!(?action, "power action");
    match action {
        IdleAction::Suspend => {
            if !inhibited(conn, INHIBIT_SUSPEND).await {
                sm(conn, "Suspend", None).await;
            }
        }
        IdleAction::Hibernate => {
            if !inhibited(conn, INHIBIT_SUSPEND).await {
                hibernate(conn).await;
            }
        }
        IdleAction::Shutdown | IdleAction::Interactive => sm(conn, "Shutdown", None).await,
        IdleAction::Logout => sm(conn, "Logout", Some(1)).await,
        IdleAction::Blank => set_power_save(conn, true).await,
        IdleAction::Nothing => {}
    }
}

async fn set_power_save(conn: &Connection, off: bool) {
    let value = zbus::zvariant::Value::from(if off { 3i32 } else { 0 });
    if let Err(err) = conn.call_method(Some(DISPLAY_CONFIG), DISPLAY_PATH, Some("org.freedesktop.DBus.Properties"), "Set", &(DISPLAY_CONFIG, "PowerSaveMode", value)).await {
        tracing::debug!(%err, "cannot change monitor power");
    }
}

async fn on_battery() -> bool {
    let Ok(system) = Connection::system().await else { return false };
    let Ok(proxy) = zbus::Proxy::new(&system, UPOWER, "/org/freedesktop/UPower", UPOWER).await else { return false };
    proxy.get_property::<bool>("OnBattery").await.unwrap_or(false)
}

#[zbus::proxy(interface = "org.gnome.Mutter.IdleMonitor", default_service = "org.gnome.Mutter.IdleMonitor", default_path = "/org/gnome/Mutter/IdleMonitor/Core")]
trait IdleMonitor {
    fn add_idle_watch(&self, interval: u64) -> zbus::Result<u32>;
    fn add_user_active_watch(&self) -> zbus::Result<u32>;
    fn remove_watch(&self, id: u32) -> zbus::Result<()>;
    #[zbus(signal)]
    fn watch_fired(&self, id: u32) -> zbus::Result<()>;
}

/// What the idle watches are for.
#[derive(Debug, Default)]
struct Watches {
    dim: Option<u32>,
    blank: Option<u32>,
    sleep: Option<(u32, IdleAction)>,
    active: Option<u32>,
}

/// Dimming, blanking and automatic suspend (run forever).
pub async fn run_idle(conn: Connection) {
    let watched = helix_settings::GSettings::new(&[POWER, SESSION]);
    let (mut changes, _watch) = watched.watch();
    let Ok(monitor) = IdleMonitorProxy::new(&conn).await else { return };
    let Ok(mut fired) = monitor.receive_watch_fired().await else { return };
    let mut watches = Watches::default();
    let mut rearm = true;
    // What we did to undo on activity: the brightness before dimming, blanked.
    let mut dimmed_from: Option<u32> = None;
    let mut blanked = false;
    let mut battery = on_battery().await;
    let mut battery_check = tokio::time::interval(Duration::from_secs(30));
    loop {
        if rearm {
            rearm = false;
            for id in [watches.dim.take(), watches.blank.take(), watches.sleep.take().map(|s| s.0)].into_iter().flatten() {
                let _ = monitor.remove_watch(id).await;
            }
            let s = read();
            let ms = |secs: u32| u64::from(secs) * 1000;
            if s.dim && s.idle_delay > 0 {
                watches.dim = monitor.add_idle_watch(ms(dim_after(s.idle_delay).unwrap_or(1))).await.ok();
            }
            if s.idle_delay > 0 {
                watches.blank = monitor.add_idle_watch(ms(s.idle_delay)).await.ok();
            }
            let (timeout, action) = if battery { s.battery } else { s.ac };
            if timeout > 0 && action != IdleAction::Nothing {
                watches.sleep = monitor.add_idle_watch(ms(timeout)).await.ok().map(|id| (id, action));
            }
            tracing::debug!(?watches, battery, "power watches");
        }
        tokio::select! {
            Some(signal) = fired.next() => {
                let Ok(args) = signal.args() else { continue };
                let id = args.id;
                if Some(id) == watches.active {
                    watches.active = None;
                    if let Some(level) = dimmed_from.take() {
                        crate::shell_keys::set_brightness_percent(&conn, level).await;
                    }
                    if blanked {
                        blanked = false;
                        set_power_save(&conn, false).await;
                    }
                    continue;
                }
                let ours = Some(id) == watches.dim || Some(id) == watches.blank || watches.sleep.is_some_and(|s| s.0 == id);
                if !ours || inhibited(&conn, INHIBIT_IDLE).await {
                    continue;
                }
                if watches.active.is_none() {
                    watches.active = monitor.add_user_active_watch().await.ok();
                }
                if Some(id) == watches.dim {
                    let s = read();
                    if let Some(current) = crate::shell_keys::brightness_percent(&conn).await {
                        if let Some(target) = dim_target(current, s.idle_brightness) {
                            tracing::info!(target, "dimming the screen");
                            crate::shell_keys::set_brightness_percent(&conn, target).await;
                            dimmed_from = Some(current);
                        }
                    }
                } else if Some(id) == watches.blank {
                    tracing::info!("turning the monitors off");
                    set_power_save(&conn, true).await;
                    blanked = true;
                } else if let Some((_, action)) = watches.sleep {
                    if action == IdleAction::Blank {
                        blanked = true;
                    }
                    act(&conn, action).await;
                }
            }
            Some(_) = changes.recv() => rearm = true,
            _ = battery_check.tick() => {
                let now = on_battery().await;
                if now != battery {
                    battery = now;
                    rearm = true;
                }
            }
            else => break,
        }
    }
}

/// Take a logind `block` lock for `what` (`handle-power-key`, …).
async fn take_lock(system: &Connection, what: &str, why: &str) -> Option<OwnedFd> {
    let reply = system.call_method(Some(LOGIND), LOGIND_PATH, Some(LOGIND_MANAGER), "Inhibit", &(what, "Helix", why, "block")).await;
    match reply.and_then(|r| r.body().deserialize::<OwnedFd>()) {
        Ok(fd) => Some(fd),
        Err(err) => {
            tracing::info!(%err, what, "cannot take the logind lock");
            None
        }
    }
}

/// Hold the power key lock, and the lid lock while an external monitor is
/// connected (run forever). `session` serves DisplayConfig's client side.
pub async fn run_logind_locks(session: Connection) {
    // Only for a session that may power off (not a nested test session).
    if !session_may(&session, "CanShutdown").await {
        tracing::info!("this session does not handle the power button or the lid");
        return;
    }
    let Ok(system) = Connection::system().await else { return };
    let _power_key = take_lock(&system, "handle-power-key", "The power button follows the power settings").await;
    let Ok(display) = zbus::Proxy::new(&session, DISPLAY_CONFIG, DISPLAY_PATH, DISPLAY_CONFIG).await else { return };
    let mut changes = display.receive_property_changed::<bool>("HasExternalMonitor").await;
    let mut lid: Option<OwnedFd> = None;
    let mut external = display.get_property::<bool>("HasExternalMonitor").await.unwrap_or(false);
    loop {
        match (external, lid.is_some()) {
            (true, false) => lid = take_lock(&system, "handle-lid-switch", "An external monitor is connected").await,
            (false, true) => lid = None,
            _ => {}
        }
        let Some(change) = changes.next().await else { break };
        external = change.get().await.unwrap_or(false);
    }
}

/// Battery warnings and the power-saver profile (run forever).
pub async fn run_battery(center: crate::notifications::Center) {
    let Ok(system) = Connection::system().await else { return };
    let Ok(device) = zbus::Proxy::new(&system, UPOWER, "/org/freedesktop/UPower/devices/DisplayDevice", "org.freedesktop.UPower.Device").await else { return };
    if !device.get_property::<bool>("IsPresent").await.unwrap_or(false) {
        return;
    }
    let mut changes = device.receive_property_changed::<u32>("WarningLevel").await;
    let mut level = device.get_property::<u32>("WarningLevel").await.unwrap_or(0);
    // The profile before switching to power-saver, if we switched.
    let mut saved_profile: Option<String> = None;
    let mut announced = 0u32;
    loop {
        let percent = device.get_property::<f64>("Percentage").await.unwrap_or(0.0).round() as i64;
        if level >= 3 && level != announced {
            announced = level;
            let critical = level >= 4;
            let (summary, body) = if critical {
                ("Battery critically low", format!("{percent} % remaining. Plug in soon, or the computer will take action."))
            } else {
                ("Battery low", format!("{percent} % remaining"))
            };
            center.system_notice(summary, &body, critical);
            if read().saver_on_low && saved_profile.is_none() {
                if let Some(current) = current_profile(&system).await.filter(|p| p != "power-saver") {
                    if crate::status::apply(&system, crate::status::Toggle::Profile("power-saver".into())).await.is_ok() {
                        tracing::info!(previous = current, "power saver for the low battery");
                        saved_profile = Some(current);
                    }
                }
            }
        }
        if level < 3 {
            announced = 0;
            if let Some(previous) = saved_profile.take() {
                // Back, unless the user changed it meanwhile.
                if current_profile(&system).await.as_deref() == Some("power-saver") {
                    let _ = crate::status::apply(&system, crate::status::Toggle::Profile(previous)).await;
                }
            }
        }
        let Some(change) = changes.next().await else { break };
        level = change.get().await.unwrap_or(0);
    }
}

async fn current_profile(system: &Connection) -> Option<String> {
    crate::status::read(Some(system)).await.profile
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimming() {
        assert_eq!(dim_after(300), Some(240));
        assert_eq!(dim_after(30), Some(20), "10 s before blanking");
        assert_eq!(dim_after(5), Some(1));
        assert_eq!(dim_after(0), None);
        assert_eq!(dim_target(80, 30), Some(30));
        assert_eq!(dim_target(20, 30), None, "already darker");
    }

    #[test]
    fn actions() {
        assert_eq!(IdleAction::parse("hibernate"), IdleAction::Hibernate);
        assert_eq!(IdleAction::parse("logout"), IdleAction::Logout);
        assert_eq!(IdleAction::parse("bogus"), IdleAction::Suspend);
    }
}
