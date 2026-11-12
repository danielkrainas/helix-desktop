//! The laptop lid (helix-desktop-spec `contracts/gsettings/power.md`,
//! "Power button and lid"): closing it with another monitor on turns the
//! built-in panel off; opening it brings the configuration back.

use futures_util::StreamExt;
use zbus::Connection;

use crate::display_config::{DisplayConfig, Layout, MonitorsConfig, OBJECT_PATH};

const UPOWER: &str = "org.freedesktop.UPower";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";

/// The current configuration without the built-in monitors, when another
/// monitor stays on (else `None`: nothing to change).
pub fn without_builtin(layout: &Layout) -> Option<MonitorsConfig> {
    let builtin: Vec<&str> = layout.monitors.iter().filter(|m| m.builtin).map(|m| m.connector.as_str()).collect();
    let mut config = layout.current_config();
    let shown = |c: &MonitorsConfig| c.logical.iter().flat_map(|l| &l.monitors).any(|m| builtin.contains(&m.connector.as_str()));
    if builtin.is_empty() || !shown(&config) {
        return None;
    }
    let primary_was_builtin = config.logical.iter().any(|l| l.primary && l.monitors.iter().any(|m| builtin.contains(&m.connector.as_str())));
    for l in &mut config.logical {
        l.monitors.retain(|m| !builtin.contains(&m.connector.as_str()));
    }
    config.logical.retain(|l| !l.monitors.is_empty());
    if config.logical.is_empty() {
        return None;
    }
    config.logical.sort_by_key(|l| (l.x, l.y));
    let (min_x, min_y) = (config.logical.iter().map(|l| l.x).min().unwrap_or(0), config.logical.iter().map(|l| l.y).min().unwrap_or(0));
    for l in &mut config.logical {
        l.x -= min_x;
        l.y -= min_y;
    }
    if primary_was_builtin || !config.logical.iter().any(|l| l.primary) {
        config.logical[0].primary = true;
    }
    Some(config)
}

/// Whether every monitor `config` uses is still connected.
pub fn still_connected(layout: &Layout, config: &MonitorsConfig) -> bool {
    config.logical.iter().flat_map(|l| &l.monitors).all(|c| layout.monitors.iter().any(|m| m.connector == c.connector))
}

async fn apply(session: &Connection, config: MonitorsConfig) {
    let Ok(iface) = session.object_server().interface::<_, DisplayConfig>(OBJECT_PATH).await else { return };
    let dc = iface.get().await;
    if let Err(err) = dc.layout().check(&config) {
        tracing::warn!(%err, "lid: cannot use that monitor configuration");
        return;
    }
    if let Err(err) = dc.apply_and_publish(config, iface.signal_emitter()).await {
        tracing::warn!(%err, "lid: cannot change the monitors");
    }
}

async fn current_layout(session: &Connection) -> Option<Layout> {
    let iface = session.object_server().interface::<_, DisplayConfig>(OBJECT_PATH).await.ok()?;
    let dc = iface.get().await;
    let layout = dc.layout().clone();
    Some(layout)
}

/// Follow the lid for the rest of the session (`session`: the connection
/// serving DisplayConfig).
pub async fn follow(session: Connection) {
    let Ok(system) = Connection::system().await else { return };
    let Ok(upower) = zbus::Proxy::new(&system, UPOWER, UPOWER_PATH, UPOWER).await else { return };
    if upower.get_property::<bool>("LidIsPresent").await != Ok(true) {
        return;
    }
    let mut changes = upower.receive_property_changed::<bool>("LidIsClosed").await;
    let mut saved: Option<MonitorsConfig> = None;
    let mut closed = false;
    while let Some(change) = changes.next().await {
        let Ok(now) = change.get().await else { continue };
        if now == closed {
            continue;
        }
        closed = now;
        let Some(layout) = current_layout(&session).await else { continue };
        if closed {
            if let Some(config) = without_builtin(&layout) {
                tracing::info!("lid closed: turning the built-in monitor off");
                saved = Some(layout.current_config());
                apply(&session, config).await;
            }
        } else if let Some(previous) = saved.take() {
            if still_connected(&layout, &previous) {
                tracing::info!("lid opened: bringing the monitors back");
                apply(&session, previous).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_config::{Mode, Monitor};

    fn monitor(connector: &str, builtin: bool, x: i32, on: bool, primary: bool) -> Monitor {
        Monitor {
            connector: connector.into(),
            vendor: String::new(),
            product: String::new(),
            serial: String::new(),
            display_name: connector.into(),
            builtin,
            modes: vec![Mode { width: 1920, height: 1080, refresh_mhz: 60_000, preferred: true, current: on }],
            x,
            y: 0,
            scale: 1.0,
            transform: 0,
            primary,
            transforms: vec![0],
        }
    }

    #[test]
    fn lid_closed_configs() {
        let docked = Layout::new(vec![monitor("eDP-1", true, 0, true, true), monitor("DP-1", false, 1920, true, false)]);
        let config = without_builtin(&docked).unwrap();
        assert_eq!(config.logical.len(), 1);
        let l = &config.logical[0];
        assert_eq!((l.x, l.y, l.primary, l.monitors[0].connector.as_str()), (0, 0, true, "DP-1"), "moved to 0,0 and made primary");
        assert!(docked.check(&config).is_ok());
        // Alone, or already off, or no built-in: nothing to change.
        assert!(without_builtin(&Layout::new(vec![monitor("eDP-1", true, 0, true, true)])).is_none());
        assert!(without_builtin(&Layout::new(vec![monitor("eDP-1", true, 0, false, false), monitor("DP-1", false, 0, true, true)])).is_none());
        assert!(without_builtin(&Layout::new(vec![monitor("DP-1", false, 0, true, true), monitor("DP-2", false, 1920, true, false)])).is_none());
        // Bringing it back needs the same monitors.
        let before = docked.current_config();
        assert!(still_connected(&docked, &before));
        assert!(!still_connected(&Layout::new(vec![monitor("eDP-1", true, 0, true, true)]), &before));
    }
}
