//! The display switcher (helix-desktop-spec `org.gnome.Mutter.DisplayConfig.md`,
//! "Display switcher"): `switch-monitor` steps through ways of using the
//! connected monitors (mirror, join, external only, built-in only); the one
//! chosen applies once the presses stop.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zbus::Connection;

use crate::display_config::{self, logical_size, supported_scales, DisplayConfig, LogicalConfig, Mode, Monitor, MonitorChoice, MonitorsConfig};

/// How long after the last press the choice applies.
pub const SETTLE: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchMode {
    Mirror,
    Join,
    External,
    Builtin,
}

pub const ORDER: [SwitchMode; 4] = [SwitchMode::Mirror, SwitchMode::Join, SwitchMode::External, SwitchMode::Builtin];

impl SwitchMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mirror => "Mirror",
            Self::Join => "Join Displays",
            Self::External => "External Only",
            Self::Builtin => "Built-in Only",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::Mirror => "view-mirror-symbolic",
            Self::Join => "video-joined-displays-symbolic",
            Self::External => "video-single-display-symbolic",
            Self::Builtin => "computer-symbolic",
        }
    }
}

fn preferred(m: &Monitor) -> Option<&Mode> {
    m.modes.iter().find(|x| x.preferred).or_else(|| m.modes.first())
}

fn choice(m: &Monitor, mode: &Mode) -> MonitorChoice {
    MonitorChoice { connector: m.connector.clone(), width: mode.width, height: mode.height, refresh_mhz: mode.refresh_mhz, color_mode: m.color_mode, underscan: m.underscan.unwrap_or(false) }
}

/// The monitor's scale if `mode` supports it, else 1.
fn scale_for(m: &Monitor, mode: &Mode) -> f64 {
    if supported_scales(mode.width, mode.height).contains(&m.scale) {
        m.scale
    } else {
        1.0
    }
}

/// `monitors` side by side, left to right, at their preferred modes; the
/// first is primary.
fn joined(monitors: &[&Monitor]) -> Option<MonitorsConfig> {
    let mut x = 0;
    let mut logical = Vec::new();
    for (i, m) in monitors.iter().enumerate() {
        let mode = preferred(m)?;
        let scale = scale_for(m, mode);
        let (w, _) = logical_size(mode.width, mode.height, scale, m.transform);
        logical.push(LogicalConfig { x, y: 0, scale, transform: m.transform, primary: i == 0, monitors: vec![choice(m, mode)] });
        x += w;
    }
    (!logical.is_empty()).then_some(MonitorsConfig { logical })
}

/// The largest size every monitor has a mode of, and each monitor's
/// fastest mode of that size.
fn common_modes<'a>(monitors: &[&'a Monitor]) -> Option<Vec<(&'a Monitor, &'a Mode)>> {
    let first = monitors.first()?;
    let mut sizes: Vec<(i32, i32)> = first.modes.iter().map(|m| (m.width, m.height)).collect();
    sizes.sort_by_key(|(w, h)| std::cmp::Reverse(i64::from(*w) * i64::from(*h)));
    sizes.dedup();
    let size = sizes.into_iter().find(|s| monitors.iter().all(|m| m.modes.iter().any(|x| (x.width, x.height) == *s)))?;
    Some(
        monitors
            .iter()
            .map(|m| {
                let mode = m.modes.iter().filter(|x| (x.width, x.height) == size).max_by_key(|x| x.refresh_mhz).expect("checked");
                (*m, mode)
            })
            .collect(),
    )
}

/// Built-in panel first, then the others from left to right.
fn ordered(monitors: &[Monitor]) -> Vec<&Monitor> {
    let mut list: Vec<&Monitor> = monitors.iter().filter(|m| !m.modes.is_empty()).collect();
    list.sort_by_key(|m| (!m.builtin, m.current_mode().is_none(), m.x, m.y));
    list
}

/// The configuration for `mode`, if these monitors allow it.
pub fn config_for(mode: SwitchMode, monitors: &[Monitor]) -> Option<MonitorsConfig> {
    let all = ordered(monitors);
    if all.len() < 2 {
        return None;
    }
    let builtin: Vec<&Monitor> = all.iter().copied().filter(|m| m.builtin).collect();
    let external: Vec<&Monitor> = all.iter().copied().filter(|m| !m.builtin).collect();
    match mode {
        SwitchMode::Join => joined(&all),
        SwitchMode::Mirror => {
            // Without a size they all have, each at its preferred mode,
            // rescaled to show the first one's picture.
            let modes = match common_modes(&all) {
                Some(modes) => modes,
                None => all.iter().map(|m| preferred(m).map(|mode| (*m, mode))).collect::<Option<Vec<_>>>()?,
            };
            let (lead, lead_mode) = modes[0];
            // Mirrored monitors share one transform: the first one's.
            Some(MonitorsConfig {
                logical: vec![LogicalConfig {
                    x: 0,
                    y: 0,
                    scale: scale_for(lead, lead_mode),
                    transform: lead.transform,
                    primary: true,
                    monitors: modes.iter().map(|(m, mode)| choice(m, mode)).collect(),
                }],
            })
        }
        SwitchMode::External if !builtin.is_empty() && !external.is_empty() => joined(&external),
        SwitchMode::Builtin if !builtin.is_empty() => joined(&builtin[..1]),
        _ => None,
    }
}

/// Which mode `config` is (`None`: none of them).
pub fn mode_of(config: &MonitorsConfig, monitors: &[Monitor]) -> Option<SwitchMode> {
    let on: Vec<&str> = config.logical.iter().flat_map(|l| l.monitors.iter().map(|m| m.connector.as_str())).collect();
    let builtin = |c: &str| monitors.iter().any(|m| m.connector == c && m.builtin);
    if config.logical.len() == 1 && on.len() > 1 {
        return Some(SwitchMode::Mirror);
    }
    let any_builtin = monitors.iter().any(|m| m.builtin && !m.modes.is_empty());
    match (on.iter().any(|c| builtin(c)), on.iter().any(|c| !builtin(c))) {
        (true, true) => Some(SwitchMode::Join),
        (false, true) if any_builtin => Some(SwitchMode::External),
        (true, false) => Some(SwitchMode::Builtin),
        _ => None,
    }
}

/// The mode after `from` that these monitors allow (from the start
/// without `from`).
pub fn next(from: Option<SwitchMode>, monitors: &[Monitor]) -> Option<SwitchMode> {
    let at = from.and_then(|f| ORDER.iter().position(|m| *m == f)).map_or(0, |i| i + 1);
    (0..ORDER.len()).map(|k| ORDER[(at + k) % ORDER.len()]).find(|m| config_for(*m, monitors).is_some())
}

/// A press of `switch-monitor` (the D-Bus side).
#[derive(Default, Clone)]
pub struct Switcher {
    /// The mode shown but not yet applied.
    chosen: Arc<Mutex<Option<SwitchMode>>>,
    generation: Arc<AtomicU64>,
}

impl Switcher {
    /// Steps to the next mode, shows it, and applies it when presses stop.
    pub async fn press(&self, conn: &Connection) {
        let Ok(iface) = conn.object_server().interface::<_, DisplayConfig>(display_config::OBJECT_PATH).await else { return };
        let (monitors, current) = {
            let display = iface.get().await;
            let layout = display.layout();
            (layout.monitors.clone(), layout.current_config())
        };
        let from = (*self.chosen.lock().unwrap_or_else(|e| e.into_inner())).or_else(|| mode_of(&current, &monitors));
        let Some(mode) = next(from, &monitors) else {
            tracing::info!("switch-monitor: one monitor; nothing to switch");
            return;
        };
        *self.chosen.lock().unwrap_or_else(|e| e.into_inner()) = Some(mode);
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        show_osd(conn, mode).await;
        let this = self.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SETTLE).await;
            if this.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            let Some(mode) = this.chosen.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
            apply(&conn, mode).await;
        });
    }
}

async fn show_osd(conn: &Connection, mode: SwitchMode) {
    let mut params: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> = std::collections::HashMap::new();
    params.insert("icon", mode.icon().into());
    params.insert("label", mode.label().into());
    if let Err(err) = conn.call_method(Some("org.gnome.Shell"), "/org/gnome/Shell", Some("org.gnome.Shell"), "ShowOSD", &(params,)).await {
        tracing::info!(%err, "no shell to show the display mode");
    }
}

async fn apply(conn: &Connection, mode: SwitchMode) {
    let Ok(iface) = conn.object_server().interface::<_, DisplayConfig>(display_config::OBJECT_PATH).await else { return };
    let (config, key) = {
        let display = iface.get().await;
        let layout = display.layout();
        (config_for(mode, &layout.monitors), layout.key())
    };
    let Some(config) = config else { return };
    tracing::info!(mode = mode.label(), "switch-monitor");
    let display = iface.get().await;
    match display.apply_and_publish(config.clone(), iface.signal_emitter()).await {
        Ok(()) => display.remember(&key, &config),
        Err(err) => tracing::warn!(err, "cannot switch the monitors"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(connector: &str, builtin: bool, sizes: &[(i32, i32)], x: i32) -> Monitor {
        Monitor {
            connector: connector.into(),
            vendor: "V".into(),
            product: "P".into(),
            serial: connector.into(),
            display_name: connector.into(),
            builtin,
            modes: sizes.iter().enumerate().map(|(i, (w, h))| Mode { width: *w, height: *h, refresh_mhz: 60_000, preferred: i == 0, current: i == 0 }).collect(),
            x,
            y: 0,
            scale: 1.0,
            transform: 0,
            primary: builtin,
            transforms: display_config::ALL_TRANSFORMS.to_vec(),
            hdr: false,
            color_mode: 0,
            underscan: None,
        }
    }

    #[test]
    fn modes_for_a_laptop_and_a_screen() {
        let ms = vec![monitor("HDMI-1", false, &[(2560, 1440), (1920, 1080)], 1920), monitor("eDP-1", true, &[(1920, 1080), (1280, 720)], 0)];
        let mirror = config_for(SwitchMode::Mirror, &ms).unwrap();
        assert_eq!(mirror.logical.len(), 1);
        assert_eq!(mirror.logical[0].monitors.iter().map(|m| (m.width, m.height)).collect::<Vec<_>>(), [(1920, 1080), (1920, 1080)]);
        let join = config_for(SwitchMode::Join, &ms).unwrap();
        assert_eq!(join.logical.iter().map(|l| (l.monitors[0].connector.as_str(), l.x, l.primary)).collect::<Vec<_>>(), [("eDP-1", 0, true), ("HDMI-1", 1920, false)]);
        let ext = config_for(SwitchMode::External, &ms).unwrap();
        assert_eq!(ext.logical[0].monitors[0].connector, "HDMI-1");
        assert_eq!(ext.logical[0].monitors[0].width, 2560);
        let inner = config_for(SwitchMode::Builtin, &ms).unwrap();
        assert_eq!(inner.logical.len(), 1);
        assert_eq!(inner.logical[0].monitors[0].connector, "eDP-1");

        assert_eq!(mode_of(&mirror, &ms), Some(SwitchMode::Mirror));
        assert_eq!(mode_of(&join, &ms), Some(SwitchMode::Join));
        assert_eq!(mode_of(&ext, &ms), Some(SwitchMode::External));
        assert_eq!(mode_of(&inner, &ms), Some(SwitchMode::Builtin));
        assert_eq!(next(Some(SwitchMode::Join), &ms), Some(SwitchMode::External));
        assert_eq!(next(Some(SwitchMode::Builtin), &ms), Some(SwitchMode::Mirror));
    }

    #[test]
    fn two_external_screens() {
        let ms = vec![monitor("DP-1", false, &[(1920, 1080)], 0), monitor("DP-2", false, &[(1280, 1024)], 1920)];
        // No common size: each at its own, rescaled.
        let mirror = config_for(SwitchMode::Mirror, &ms).unwrap();
        assert_eq!(mirror.logical[0].monitors.iter().map(|m| (m.width, m.height)).collect::<Vec<_>>(), [(1920, 1080), (1280, 1024)]);
        assert_eq!(config_for(SwitchMode::External, &ms), None);
        assert_eq!(next(None, &ms), Some(SwitchMode::Mirror));
        assert_eq!(next(Some(SwitchMode::Mirror), &ms), Some(SwitchMode::Join));
        assert_eq!(next(Some(SwitchMode::Join), &ms), Some(SwitchMode::Mirror));
        assert_eq!(next(None, &ms[..1]), None, "one monitor");
    }
}
