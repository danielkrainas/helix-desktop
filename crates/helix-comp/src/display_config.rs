//! `org.gnome.Mutter.DisplayConfig` (read side).
//!
//! Contract: `helix-desktop-spec/contracts/dbus/org.gnome.Mutter.DisplayConfig.xml`
//! and `org.gnome.Mutter.DisplayConfig.md`.
//!
//! The compositor describes its outputs as [`Monitor`]s; this module turns
//! them into the DisplayConfig wire format. Changing the configuration over
//! D-Bus is not supported yet (`ApplyMonitorsConfigAllowed` is false).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Str};

pub const BUS_NAME: &str = "org.gnome.Mutter.DisplayConfig";
pub const OBJECT_PATH: &str = "/org/gnome/Mutter/DisplayConfig";

/// `layout-mode`: logical monitor sizes are scaled.
pub const LAYOUT_MODE_LOGICAL: u32 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct Mode {
    pub width: i32,
    pub height: i32,
    /// Refresh rate in millihertz.
    pub refresh_mhz: i32,
    pub preferred: bool,
    pub current: bool,
}

impl Mode {
    pub fn id(&self) -> String {
        format!("{}x{}@{:.3}", self.width, self.height, self.refresh_hz())
    }

    pub fn refresh_hz(&self) -> f64 {
        f64::from(self.refresh_mhz) / 1000.0
    }
}

/// One physical monitor and the logical monitor showing it.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    pub connector: String,
    pub vendor: String,
    pub product: String,
    pub serial: String,
    pub display_name: String,
    pub builtin: bool,
    pub modes: Vec<Mode>,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    /// Transform, per the DisplayConfig notes (0 = normal).
    pub transform: u32,
    pub primary: bool,
}

impl Monitor {
    fn spec(&self) -> MonitorSpec {
        (
            self.connector.clone(),
            self.vendor.clone(),
            self.product.clone(),
            self.serial.clone(),
        )
    }

    fn current_mode(&self) -> Option<&Mode> {
        self.modes.iter().find(|m| m.current)
    }
}

type Props = HashMap<String, OwnedValue>;
type MonitorSpec = (String, String, String, String);
type WireMode = (String, i32, i32, f64, f64, Vec<f64>, Props);
type WireMonitor = (MonitorSpec, Vec<WireMode>, Props);
type WireLogicalMonitor = (i32, i32, f64, u32, bool, Vec<MonitorSpec>, Props);
type WireCrtc = (u32, i64, i32, i32, i32, i32, i32, u32, Vec<u32>, Props);
type WireOutput = (u32, i64, i32, Vec<u32>, String, Vec<u32>, Vec<u32>, Props);
type WireLegacyMode = (u32, i64, u32, u32, f64, u32);

fn props<const N: usize>(entries: [(&str, OwnedValue); N]) -> Props {
    entries.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

fn string(s: &str) -> OwnedValue {
    OwnedValue::from(Str::from(s.to_owned()))
}

/// The current monitor layout plus its serial.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub serial: u32,
    pub monitors: Vec<Monitor>,
}

impl Layout {
    pub fn new(monitors: Vec<Monitor>) -> Self {
        Self {
            serial: 1,
            monitors,
        }
    }

    /// Replace the monitors; returns true (and bumps the serial) if anything
    /// changed.
    pub fn update(&mut self, monitors: Vec<Monitor>) -> bool {
        if monitors == self.monitors {
            return false;
        }
        self.monitors = monitors;
        self.serial = self.serial.wrapping_add(1).max(1);
        true
    }

    pub fn has_external_monitor(&self) -> bool {
        self.monitors.iter().any(|m| !m.builtin)
    }

    pub fn current_state(&self) -> (u32, Vec<WireMonitor>, Vec<WireLogicalMonitor>, Props) {
        let monitors = self
            .monitors
            .iter()
            .map(|m| {
                let modes = m
                    .modes
                    .iter()
                    .map(|mode| {
                        let mut supported = vec![1.0];
                        if (m.scale - 1.0).abs() > f64::EPSILON {
                            supported.push(m.scale);
                        }
                        (
                            mode.id(),
                            mode.width,
                            mode.height,
                            mode.refresh_hz(),
                            1.0,
                            supported,
                            props([
                                ("is-current", OwnedValue::from(mode.current)),
                                ("is-preferred", OwnedValue::from(mode.preferred)),
                                ("is-interlaced", OwnedValue::from(false)),
                            ]),
                        )
                    })
                    .collect();
                let mprops = props([
                    ("is-builtin", OwnedValue::from(m.builtin)),
                    ("display-name", string(&m.display_name)),
                ]);
                (m.spec(), modes, mprops)
            })
            .collect();
        let logical = self
            .monitors
            .iter()
            .filter(|m| m.current_mode().is_some())
            .map(|m| (m.x, m.y, m.scale, m.transform, m.primary, vec![m.spec()], Props::new()))
            .collect();
        let properties = props([
            ("layout-mode", OwnedValue::from(LAYOUT_MODE_LOGICAL)),
            ("supports-changing-layout-mode", OwnedValue::from(false)),
            ("global-scale-required", OwnedValue::from(false)),
        ]);
        (self.serial, monitors, logical, properties)
    }

    /// Legacy view: one CRTC and one output per monitor.
    pub fn resources(&self) -> (u32, Vec<WireCrtc>, Vec<WireOutput>, Vec<WireLegacyMode>, i32, i32) {
        let mut crtcs = Vec::new();
        let mut outputs = Vec::new();
        let mut modes = Vec::new();
        let (mut max_w, mut max_h) = (0, 0);
        for (i, m) in self.monitors.iter().enumerate() {
            let id = i as u32;
            let first_mode = modes.len() as u32;
            let mut current = -1;
            for (j, mode) in m.modes.iter().enumerate() {
                let mode_id = first_mode + j as u32;
                if mode.current {
                    current = mode_id as i32;
                }
                modes.push((
                    mode_id,
                    i64::from(mode_id),
                    mode.width as u32,
                    mode.height as u32,
                    mode.refresh_hz(),
                    0,
                ));
            }
            let (w, h) = m
                .current_mode()
                .map(|c| (c.width, c.height))
                .unwrap_or((0, 0));
            max_w = max_w.max(m.x + w);
            max_h = max_h.max(m.y + h);
            crtcs.push((
                id,
                i64::from(id),
                m.x,
                m.y,
                w,
                h,
                current,
                m.transform,
                (0..8).collect(),
                Props::new(),
            ));
            outputs.push((
                id,
                i64::from(id),
                if current >= 0 { id as i32 } else { -1 },
                vec![id],
                m.connector.clone(),
                (first_mode..modes.len() as u32).collect(),
                vec![],
                props([
                    ("vendor", string(&m.vendor)),
                    ("product", string(&m.product)),
                    ("serial", string(&m.serial)),
                    ("display-name", string(&m.display_name)),
                    ("primary", OwnedValue::from(m.primary)),
                ]),
            ));
        }
        (self.serial, crtcs, outputs, modes, max_w, max_h)
    }
}

/// The exported DisplayConfig object.
pub struct DisplayConfig {
    layout: Arc<Mutex<Layout>>,
    power_save_mode: i32,
}

impl DisplayConfig {
    pub fn new(layout: Arc<Mutex<Layout>>) -> Self {
        Self {
            layout,
            power_save_mode: 0,
        }
    }

    fn layout(&self) -> MutexGuard<'_, Layout> {
        self.layout.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[interface(name = "org.gnome.Mutter.DisplayConfig")]
impl DisplayConfig {
    #[zbus(out_args("serial", "crtcs", "outputs", "modes", "max_screen_width", "max_screen_height"))]
    async fn get_resources(
        &self,
    ) -> (u32, Vec<WireCrtc>, Vec<WireOutput>, Vec<WireLegacyMode>, i32, i32) {
        self.layout().resources()
    }

    #[zbus(out_args("serial", "monitors", "logical_monitors", "properties"))]
    async fn get_current_state(
        &self,
    ) -> (u32, Vec<WireMonitor>, Vec<WireLogicalMonitor>, Props) {
        self.layout().current_state()
    }

    #[zbus(signal)]
    pub async fn monitors_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property)]
    async fn backlight(&self) -> (u32, Vec<Props>) {
        // No backlight control yet.
        (self.layout().serial, Vec::new())
    }

    #[zbus(property)]
    async fn power_save_mode(&self) -> i32 {
        self.power_save_mode
    }

    #[zbus(property)]
    async fn set_power_save_mode(&mut self, mode: i32) -> zbus::fdo::Result<()> {
        if !(0..=3).contains(&mode) {
            return Err(zbus::fdo::Error::InvalidArgs(format!("invalid power save mode {mode}")));
        }
        // Recorded only: the nested backend cannot blank outputs.
        self.power_save_mode = mode;
        Ok(())
    }

    #[zbus(property)]
    async fn panel_orientation_managed(&self) -> bool {
        false
    }

    #[zbus(property)]
    async fn apply_monitors_config_allowed(&self) -> bool {
        false
    }

    #[zbus(property)]
    async fn night_light_supported(&self) -> bool {
        false
    }

    #[zbus(property)]
    async fn has_external_monitor(&self) -> bool {
        self.layout().has_external_monitor()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn nested(width: i32, height: i32) -> Monitor {
        Monitor {
            connector: "WINIT-1".into(),
            vendor: "Helix".into(),
            product: "Nested".into(),
            serial: "0".into(),
            display_name: "Nested window".into(),
            builtin: false,
            modes: vec![Mode {
                width,
                height,
                refresh_mhz: 60_000,
                preferred: true,
                current: true,
            }],
            x: 0,
            y: 0,
            scale: 1.0,
            transform: 0,
            primary: true,
        }
    }

    #[test]
    fn serial_changes_only_on_real_changes() {
        let mut layout = Layout::new(vec![nested(800, 600)]);
        assert_eq!(layout.serial, 1);
        assert!(!layout.update(vec![nested(800, 600)]));
        assert_eq!(layout.serial, 1);
        assert!(layout.update(vec![nested(1024, 768)]));
        assert_eq!(layout.serial, 2);
    }

    #[test]
    fn current_state_shape() {
        let layout = Layout::new(vec![nested(800, 600)]);
        let (serial, monitors, logical, props) = layout.current_state();
        assert_eq!(serial, 1);
        assert_eq!(monitors.len(), 1);
        let (spec, modes, mprops) = &monitors[0];
        assert_eq!(spec.0, "WINIT-1");
        assert_eq!(modes[0].0, "800x600@60.000");
        assert_eq!(modes[0].3, 60.0);
        assert!(bool::try_from(&*modes[0].6["is-current"]).unwrap());
        assert!(!bool::try_from(&*mprops["is-builtin"]).unwrap());
        assert_eq!(logical.len(), 1);
        assert_eq!(logical[0].5, vec![spec.clone()]);
        assert!(logical[0].4, "primary");
        assert_eq!(u32::try_from(&*props["layout-mode"]).unwrap(), LAYOUT_MODE_LOGICAL);
        assert!(layout.has_external_monitor());
    }

    #[test]
    fn resources_shape() {
        let mut second = nested(1920, 1080);
        second.connector = "WINIT-2".into();
        second.x = 800;
        second.primary = false;
        let layout = Layout::new(vec![nested(800, 600), second]);
        let (_, crtcs, outputs, modes, max_w, max_h) = layout.resources();
        assert_eq!(crtcs.len(), 2);
        assert_eq!(outputs[1].4, "WINIT-2");
        assert_eq!(outputs[1].5, vec![1]);
        assert_eq!(crtcs[1].6, 1, "current mode index");
        assert_eq!(modes.len(), 2);
        assert_eq!((max_w, max_h), (2720, 1080));
    }
}
