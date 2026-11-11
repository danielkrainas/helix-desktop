//! `org.gnome.Mutter.DisplayConfig`.
//!
//! Contract: `helix-desktop-spec/contracts/dbus/org.gnome.Mutter.DisplayConfig.xml`
//! and `org.gnome.Mutter.DisplayConfig.md`.
//!
//! The compositor describes its outputs as [`Monitor`]s; this module turns
//! them into the DisplayConfig wire format. `ApplyMonitorsConfig` is
//! checked here ([`Layout::parse`]) and applied by the compositor, which
//! receives an [`ApplyRequest`]; persistent configurations are kept in a
//! [`Store`].

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use futures_util::StreamExt;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Str};
use zbus::{fdo, interface, Connection, MatchRule, MessageStream};

pub const BUS_NAME: &str = "org.gnome.Mutter.DisplayConfig";
pub const OBJECT_PATH: &str = "/org/gnome/Mutter/DisplayConfig";

/// `layout-mode`: logical monitor sizes are scaled.
pub const LAYOUT_MODE_LOGICAL: u32 = 1;

/// The shell's "keep these settings?" question
/// (helix-desktop-spec `org.helix.DisplayChangeDialog.md`).
const DIALOG_BUS_NAME: &str = "org.gnome.Shell";
const DIALOG_PATH: &str = "/org/helix/DisplayChangeDialog";
const DIALOG_IFACE: &str = "org.helix.DisplayChangeDialog";
/// How long the user has to keep a change.
pub const CONFIRM_SECONDS: u32 = 20;
/// Extra time before giving up on a dialog that never answers.
const CONFIRM_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Every transform (0 normal … 7 flipped 270°).
pub const ALL_TRANSFORMS: [u32; 8] = [0, 1, 2, 3, 4, 5, 6, 7];

/// Scales from 1 to 4 in steps of 0.25 that leave a whole number of
/// logical pixels each way and at least 800×480 of them; 1 always.
pub fn supported_scales(width: i32, height: i32) -> Vec<f64> {
    // Scale k/4: the logical size is 4·width/k × 4·height/k.
    let (w4, h4) = (i64::from(width) * 4, i64::from(height) * 4);
    (4..=16)
        .filter(|&k| k == 4 || (w4 % k == 0 && h4 % k == 0 && (w4 / k) * (h4 / k) >= 800 * 480))
        .map(|k| k as f64 / 4.0)
        .collect()
}

/// Logical size of a `width`×`height` mode at `scale` with `transform`.
pub fn logical_size(width: i32, height: i32, scale: f64, transform: u32) -> (i32, i32) {
    let w = (f64::from(width) / scale).round() as i32;
    let h = (f64::from(height) / scale).round() as i32;
    if transform % 2 == 1 {
        (h, w)
    } else {
        (w, h)
    }
}

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
    /// Transforms this monitor can show.
    pub transforms: Vec<u32>,
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

/// The default (SDR) color mode; the only one Helix drives monitors in.
pub const COLOR_MODE_DEFAULT: u32 = 0;

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

    fn monitor(&self, connector: &str) -> Option<&Monitor> {
        self.monitors.iter().find(|m| m.connector == connector)
    }

    /// Identifies this set of monitors (for remembered configurations).
    pub fn key(&self) -> String {
        let mut ids: Vec<String> = self
            .monitors
            .iter()
            .map(|m| format!("{}|{}|{}|{}", m.connector, m.vendor, m.product, m.serial))
            .collect();
        ids.sort();
        ids.join(";")
    }

    /// Turn `ApplyMonitorsConfig` arguments into a checked configuration.
    pub fn parse(&self, logical: &[WireLogicalConfig], properties: &Props) -> Result<MonitorsConfig, String> {
        if let Some(mode) = properties.get("layout-mode") {
            let mode = u32::try_from(&**mode).map_err(|_| "layout-mode must be a u32".to_owned())?;
            if mode != LAYOUT_MODE_LOGICAL {
                return Err(format!("layout mode {mode} is not supported"));
            }
        }
        let mut config = MonitorsConfig::default();
        for (x, y, scale, transform, primary, monitors) in logical {
            let mut choices = Vec::new();
            for (connector, mode_id, monitor_properties) in monitors {
                if let Some(mode) = monitor_properties.get("color-mode") {
                    if u32::try_from(&**mode) != Ok(COLOR_MODE_DEFAULT) {
                        return Err(format!("{connector} cannot use that color mode"));
                    }
                }
                let monitor = self
                    .monitor(connector)
                    .ok_or_else(|| format!("no monitor is connected at {connector}"))?;
                let mode = monitor
                    .modes
                    .iter()
                    .find(|m| m.id() == *mode_id)
                    .ok_or_else(|| format!("{connector} has no mode {mode_id}"))?;
                choices.push(MonitorChoice {
                    connector: connector.clone(),
                    width: mode.width,
                    height: mode.height,
                    refresh_mhz: mode.refresh_mhz,
                });
            }
            config.logical.push(LogicalConfig {
                x: *x,
                y: *y,
                scale: *scale,
                transform: *transform,
                primary: *primary,
                monitors: choices,
            });
        }
        self.check(&config)?;
        Ok(config)
    }

    /// Whether `config` is valid for these monitors (see the contract notes).
    pub fn check(&self, config: &MonitorsConfig) -> Result<(), String> {
        if config.logical.is_empty() {
            return Err("at least one monitor must stay on".into());
        }
        let mut seen = HashSet::new();
        let mut rects = Vec::new();
        for l in &config.logical {
            let first = l.monitors.first().ok_or("a logical monitor shows no monitor")?;
            if !ALL_TRANSFORMS.contains(&l.transform) {
                return Err(format!("invalid transform {}", l.transform));
            }
            for c in &l.monitors {
                let monitor = self
                    .monitor(&c.connector)
                    .ok_or_else(|| format!("no monitor is connected at {}", c.connector))?;
                if !seen.insert(c.connector.as_str()) {
                    return Err(format!("{} appears more than once", c.connector));
                }
                if !monitor.modes.iter().any(|m| c.is(m)) {
                    return Err(format!("{} has no {}x{} mode at {} mHz", c.connector, c.width, c.height, c.refresh_mhz));
                }
                if (c.width, c.height) != (first.width, first.height) {
                    return Err("mirrored monitors must use modes of the same size".into());
                }
                if !monitor.transforms.contains(&l.transform) {
                    return Err(format!("{} cannot show transform {}", c.connector, l.transform));
                }
            }
            if !supported_scales(first.width, first.height).iter().any(|s| (s - l.scale).abs() < 1e-6) {
                return Err(format!("scale {} is not supported for {}x{}", l.scale, first.width, first.height));
            }
            let (w, h) = logical_size(first.width, first.height, l.scale, l.transform);
            rects.push(Rect { x: l.x, y: l.y, w, h });
        }
        let primaries = config.logical.iter().filter(|l| l.primary).count();
        if primaries != 1 {
            return Err(format!("exactly one logical monitor must be primary, not {primaries}"));
        }
        for (i, a) in rects.iter().enumerate() {
            if rects[i + 1..].iter().any(|b| a.overlaps(b)) {
                return Err("logical monitors overlap".into());
            }
        }
        let min_x = rects.iter().map(|r| r.x).min().unwrap_or(0);
        let min_y = rects.iter().map(|r| r.y).min().unwrap_or(0);
        if (min_x, min_y) != (0, 0) {
            return Err(format!("the layout must start at 0,0, not {min_x},{min_y}"));
        }
        let mut reached = vec![false; rects.len()];
        reached[0] = true;
        let mut todo = vec![0];
        while let Some(i) = todo.pop() {
            for j in 0..rects.len() {
                if !reached[j] && rects[i].touches(&rects[j]) {
                    reached[j] = true;
                    todo.push(j);
                }
            }
        }
        if reached.contains(&false) {
            return Err("every logical monitor must touch another one".into());
        }
        Ok(())
    }

    /// The monitors as they look with `config` applied.
    pub fn applied(&self, config: &MonitorsConfig) -> Vec<Monitor> {
        self.monitors
            .iter()
            .map(|m| {
                let mut m = m.clone();
                let found = config
                    .logical
                    .iter()
                    .find_map(|l| l.monitors.iter().find(|c| c.connector == m.connector).map(|c| (l, c)));
                for mode in &mut m.modes {
                    mode.current = found.is_some_and(|(_, c)| c.is(mode));
                }
                match found {
                    Some((l, _)) => {
                        m.x = l.x;
                        m.y = l.y;
                        m.scale = l.scale;
                        m.transform = l.transform;
                        m.primary = l.primary;
                    }
                    None => m.primary = false,
                }
                m
            })
            .collect()
    }

    /// Whether `config` is what is shown now.
    pub fn is_current(&self, config: &MonitorsConfig) -> bool {
        self.applied(config) == self.monitors
    }

    /// What is shown now, as a configuration.
    pub fn current_config(&self) -> MonitorsConfig {
        let mut config = MonitorsConfig::default();
        for m in &self.monitors {
            let Some(mode) = m.current_mode() else { continue };
            let choice = MonitorChoice {
                connector: m.connector.clone(),
                width: mode.width,
                height: mode.height,
                refresh_mhz: mode.refresh_mhz,
            };
            let same = |l: &&mut LogicalConfig| l.x == m.x && l.y == m.y && l.scale == m.scale && l.transform == m.transform;
            match config.logical.iter_mut().find(same) {
                Some(l) => {
                    l.primary |= m.primary;
                    l.monitors.push(choice);
                }
                None => config.logical.push(LogicalConfig {
                    x: m.x,
                    y: m.y,
                    scale: m.scale,
                    transform: m.transform,
                    primary: m.primary,
                    monitors: vec![choice],
                }),
            }
        }
        config
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
                        let supported = supported_scales(mode.width, mode.height);
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
                    // SDR only (no HDR output).
                    ("color-mode", OwnedValue::from(COLOR_MODE_DEFAULT)),
                    (
                        "supported-color-modes",
                        OwnedValue::try_from(zbus::zvariant::Value::from(vec![COLOR_MODE_DEFAULT])).expect("array"),
                    ),
                ]);
                (m.spec(), modes, mprops)
            })
            .collect();
        // Monitors shown at the same place, scale and transform mirror each
        // other: one logical monitor.
        let mut logical: Vec<WireLogicalMonitor> = Vec::new();
        for m in self.monitors.iter().filter(|m| m.current_mode().is_some()) {
            let same = |l: &&mut WireLogicalMonitor| l.0 == m.x && l.1 == m.y && l.2 == m.scale && l.3 == m.transform;
            match logical.iter_mut().find(same) {
                Some(l) => {
                    l.4 |= m.primary;
                    l.5.push(m.spec());
                }
                None => logical.push((m.x, m.y, m.scale, m.transform, m.primary, vec![m.spec()], Props::new())),
            }
        }
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
                m.transforms.clone(),
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

#[derive(Debug, Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn overlaps(&self, o: &Rect) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }

    /// Shares part of an edge.
    fn touches(&self, o: &Rect) -> bool {
        let rows = self.y < o.y + o.h && o.y < self.y + self.h;
        let columns = self.x < o.x + o.w && o.x < self.x + self.w;
        ((self.x + self.w == o.x || o.x + o.w == self.x) && rows)
            || ((self.y + self.h == o.y || o.y + o.h == self.y) && columns)
    }
}

/// One monitor of a logical monitor, and the mode to show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorChoice {
    pub connector: String,
    pub width: i32,
    pub height: i32,
    pub refresh_mhz: i32,
}

impl MonitorChoice {
    fn is(&self, mode: &Mode) -> bool {
        (mode.width, mode.height, mode.refresh_mhz) == (self.width, self.height, self.refresh_mhz)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogicalConfig {
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub transform: u32,
    pub primary: bool,
    pub monitors: Vec<MonitorChoice>,
}

/// A whole monitor configuration; monitors it doesn't mention are off.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MonitorsConfig {
    pub logical: Vec<LogicalConfig>,
}

/// `ApplyMonitorsConfig`'s `method`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Verify,
    Temporary,
    Persistent,
}

impl TryFrom<u32> for Method {
    type Error = String;

    fn try_from(value: u32) -> Result<Self, String> {
        match value {
            0 => Ok(Self::Verify),
            1 => Ok(Self::Temporary),
            2 => Ok(Self::Persistent),
            other => Err(format!("unknown method {other}")),
        }
    }
}

/// Asks the compositor to apply a configuration; it answers with the
/// resulting monitors.
#[derive(Debug)]
pub struct ApplyRequest {
    pub config: MonitorsConfig,
    pub reply: oneshot::Sender<Result<Vec<Monitor>, String>>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    configuration: Vec<StoredConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredConfig {
    /// [`Layout::key`] of the monitors it is for.
    monitors: String,
    logical: Vec<LogicalConfig>,
}

/// Remembered (persistent) configurations, one per set of monitors.
#[derive(Debug, Clone, Default)]
pub struct Store {
    path: Option<PathBuf>,
}

impl Store {
    pub fn at(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// `$XDG_CONFIG_HOME/helix/monitors.toml`.
    pub fn user() -> Self {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
        Self {
            path: base.map(|b| b.join("helix").join("monitors.toml")),
        }
    }

    fn read(&self) -> StoreFile {
        let Some(path) = &self.path else { return StoreFile::default() };
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|err| {
                tracing::warn!(%err, path = %path.display(), "ignoring unreadable monitor configurations");
                StoreFile::default()
            }),
            Err(_) => StoreFile::default(),
        }
    }

    pub fn load(&self, key: &str) -> Option<MonitorsConfig> {
        self.read()
            .configuration
            .into_iter()
            .find(|c| c.monitors == key)
            .map(|c| MonitorsConfig { logical: c.logical })
    }

    pub fn save(&self, key: &str, config: &MonitorsConfig) -> Result<(), String> {
        let Some(path) = &self.path else { return Ok(()) };
        let mut file = self.read();
        file.configuration.retain(|c| c.monitors != key);
        file.configuration.push(StoredConfig {
            monitors: key.to_owned(),
            logical: config.logical.clone(),
        });
        let text = toml::to_string(&file).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }
}

/// Gamma ramps: red, green, blue.
pub type Ramps = (Vec<u16>, Vec<u16>, Vec<u16>);

/// A color transformation matrix: row-major, each value S31.32 sign and
/// magnitude (the kernel's `drm_color_ctm`).
pub type Ctm = [u64; 9];

/// The identity matrix.
pub const IDENTITY_CTM: Ctm = {
    const ONE: u64 = 1 << 32;
    [ONE, 0, 0, 0, ONE, 0, 0, 0, ONE]
};

/// A color change for a monitor.
#[derive(Debug, Clone, PartialEq)]
pub enum ColorSet {
    Gamma(Ramps),
    Ctm(Ctm),
}

/// Why a color request failed.
#[derive(Debug, Clone, PartialEq)]
pub enum ColorError {
    Invalid(String),
    NotSupported(String),
}

impl From<ColorError> for fdo::Error {
    fn from(err: ColorError) -> Self {
        match err {
            ColorError::Invalid(msg) => fdo::Error::InvalidArgs(msg),
            ColorError::NotSupported(msg) => fdo::Error::NotSupported(msg),
        }
    }
}

/// Ask the compositor for a monitor's gamma ramps (`set` None) or to
/// change its colors; the reply is the gamma ramps in effect.
#[derive(Debug)]
pub struct ColorRequest {
    pub connector: String,
    pub set: Option<ColorSet>,
    pub reply: oneshot::Sender<Result<Ramps, ColorError>>,
}

/// Identity ramps of `size` entries.
pub fn identity_ramps(size: usize) -> Ramps {
    let ramp: Vec<u16> = (0..size)
        .map(|i| if size > 1 { (i * 65535 / (size - 1)) as u16 } else { 65535 })
        .collect();
    (ramp.clone(), ramp.clone(), ramp)
}

/// A persistent change waiting for the user to keep it.
#[derive(Debug)]
struct Pending {
    /// What to go back to: the configuration before the first unconfirmed
    /// change.
    previous: MonitorsConfig,
    generation: u64,
}

/// The exported DisplayConfig object.
pub struct DisplayConfig {
    layout: Arc<Mutex<Layout>>,
    power_save_mode: i32,
    /// The compositor, if it accepts configuration changes.
    applier: Option<mpsc::UnboundedSender<ApplyRequest>>,
    store: Store,
    /// Countdown for keeping a persistent change; 0 keeps it without asking.
    confirm_seconds: u32,
    pending: Mutex<Option<Pending>>,
    generation: std::sync::atomic::AtomicU64,
    /// The built-in panel's backlight, if it can be controlled.
    backlight: Option<crate::backlight::Backlight>,
    /// The compositor, if it handles gamma.
    gamma: Option<mpsc::UnboundedSender<ColorRequest>>,
    /// The value last published, to notice outside changes.
    backlight_shown: Mutex<Option<i32>>,
}

impl DisplayConfig {
    pub fn new(
        layout: Arc<Mutex<Layout>>,
        applier: Option<mpsc::UnboundedSender<ApplyRequest>>,
        store: Store,
        confirm_seconds: u32,
        backlight: Option<crate::backlight::Backlight>,
        gamma: Option<mpsc::UnboundedSender<ColorRequest>>,
    ) -> Self {
        Self {
            layout,
            power_save_mode: 0,
            applier,
            store,
            confirm_seconds,
            pending: Mutex::new(None),
            generation: Default::default(),
            backlight,
            backlight_shown: Mutex::new(None),
            gamma,
        }
    }

    /// Send a color request for `crtc` (a `GetResources` CRTC or output id;
    /// they are the same) to the compositor.
    async fn color(&self, serial: u32, crtc: u32, set: Option<ColorSet>) -> fdo::Result<Ramps> {
        let connector = {
            let layout = self.layout();
            if serial != layout.serial {
                return Err(fdo::Error::InvalidArgs(format!("stale serial {serial}")));
            }
            let monitor = layout.monitors.get(crtc as usize);
            monitor.map(|m| m.connector.clone()).ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown CRTC or output {crtc}")))?
        };
        let sender = self.gamma.as_ref().ok_or_else(|| fdo::Error::NotSupported("no gamma control".into()))?;
        let (reply, answer) = oneshot::channel();
        sender
            .send(ColorRequest { connector, set, reply })
            .map_err(|_| fdo::Error::Failed("the compositor is gone".into()))?;
        answer
            .await
            .map_err(|_| fdo::Error::Failed("the compositor did not answer".into()))?
            .map_err(fdo::Error::from)
    }

    /// The controllable built-in monitor's connector, its device and value.
    fn builtin_backlight(&self) -> Option<(String, crate::backlight::Device, i32)> {
        let backlight = self.backlight.as_ref()?;
        let connector = {
            let layout = self.layout();
            let monitor = layout.monitors.iter().find(|m| m.builtin && m.current_mode().is_some())?;
            monitor.connector.clone()
        };
        let device = backlight.device()?;
        let value = backlight.value(&device)?;
        Some((connector, device, value))
    }

    /// `Backlight`: one entry per monitor.
    pub fn backlight_entries(&self) -> (u32, Vec<Props>) {
        let controlled = self.builtin_backlight();
        *self.backlight_shown.lock().unwrap_or_else(|e| e.into_inner()) = controlled.as_ref().map(|c| c.2);
        let layout = self.layout();
        let entries = layout
            .monitors
            .iter()
            .map(|m| {
                let mut entry = props([
                    ("connector", string(&m.connector)),
                    ("active", OwnedValue::from(m.current_mode().is_some())),
                ]);
                if let Some((connector, device, value)) = &controlled {
                    if *connector == m.connector {
                        entry.insert("min".into(), OwnedValue::from(crate::backlight::MIN));
                        entry.insert("max".into(), OwnedValue::from(device.max));
                        entry.insert("value".into(), OwnedValue::from(*value));
                    }
                }
                entry
            })
            .collect();
        (layout.serial, entries)
    }

    /// Whether the brightness changed since it was last published.
    pub fn backlight_moved(&self) -> bool {
        let now = self.builtin_backlight().map(|c| c.2);
        *self.backlight_shown.lock().unwrap_or_else(|e| e.into_inner()) != now
    }

    fn layout(&self) -> MutexGuard<'_, Layout> {
        self.layout.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pending(&self) -> MutexGuard<'_, Option<Pending>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// After a persistent change: ask the shell to confirm it, and remember
    /// or revert it depending on the answer.
    async fn confirm(&self, conn: &Connection, previous: MonitorsConfig, key: String, config: MonitorsConfig) {
        let signals = match open_dialog(conn, self.confirm_seconds).await {
            Ok(signals) => signals,
            Err(err) => {
                tracing::info!(%err, "no display change dialog; keeping the change");
                if let Err(err) = self.store.save(&key, &config) {
                    tracing::warn!(%err, "cannot remember the monitor configuration");
                }
                return;
            }
        };
        let generation = self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        *self.pending() = Some(Pending { previous, generation });
        let timeout = std::time::Duration::from_secs(u64::from(self.confirm_seconds)) + CONFIRM_GRACE;
        let conn = conn.clone();
        tokio::spawn(async move {
            let keep = wait_for_answer(signals, timeout).await;
            if keep.is_none() {
                close_dialog(&conn).await;
            }
            let Ok(iface) = conn.object_server().interface::<_, DisplayConfig>(OBJECT_PATH).await else { return };
            let dc = iface.get().await;
            let previous = {
                let mut pending = dc.pending();
                match pending.as_ref() {
                    Some(p) if p.generation == generation => pending.take().map(|p| p.previous),
                    // Superseded by a newer change.
                    _ => return,
                }
            };
            let Some(previous) = previous else { return };
            if keep == Some(true) {
                tracing::info!("display change kept");
                if let Err(err) = dc.store.save(&key, &config) {
                    tracing::warn!(%err, "cannot remember the monitor configuration");
                }
            } else {
                tracing::info!(timed_out = keep.is_none(), "reverting the display change");
                if let Err(err) = dc.apply_and_publish(previous, iface.signal_emitter()).await {
                    tracing::warn!(%err, "cannot revert the display change");
                }
            }
        });
    }

    /// Have the compositor apply `config`, then publish the new layout.
    pub async fn apply_and_publish(&self, config: MonitorsConfig, emitter: &SignalEmitter<'_>) -> Result<(), String> {
        let applier = self.applier.as_ref().ok_or("the compositor cannot change monitors")?;
        let (reply, answer) = oneshot::channel();
        applier
            .send(ApplyRequest { config, reply })
            .map_err(|_| "the compositor is gone".to_owned())?;
        let monitors = answer.await.map_err(|_| "the compositor did not answer".to_owned())??;
        let changed = self.layout().update(monitors);
        if changed {
            Self::monitors_changed(emitter).await.map_err(|e| e.to_string())?;
            self.has_external_monitor_changed(emitter).await.map_err(|e| e.to_string())?;
            self.backlight_changed(emitter).await.map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// The set of monitors changed (or the compositor started): bring back
    /// the configuration remembered for it, if any.
    pub async fn restore_saved(&self, emitter: &SignalEmitter<'_>) {
        if self.applier.is_none() {
            return;
        }
        let config = {
            let layout = self.layout();
            let Some(config) = self.store.load(&layout.key()) else { return };
            if let Err(err) = layout.check(&config) {
                tracing::warn!(%err, "ignoring the remembered monitor configuration");
                return;
            }
            if layout.is_current(&config) {
                return;
            }
            config
        };
        tracing::info!("restoring the remembered monitor configuration");
        if let Err(err) = self.apply_and_publish(config, emitter).await {
            tracing::warn!(%err, "cannot restore the remembered monitor configuration");
        }
    }
}

/// Subscribe to the dialog's answers, then open it.
async fn open_dialog(conn: &Connection, seconds: u32) -> zbus::Result<MessageStream> {
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(DIALOG_IFACE)?
        .path(DIALOG_PATH)?
        .build();
    let signals = MessageStream::for_match_rule(rule, conn, None).await?;
    conn.call_method(Some(DIALOG_BUS_NAME), DIALOG_PATH, Some(DIALOG_IFACE), "Open", &(seconds,))
        .await?;
    Ok(signals)
}

async fn close_dialog(conn: &Connection) {
    let _ = conn
        .call_method(Some(DIALOG_BUS_NAME), DIALOG_PATH, Some(DIALOG_IFACE), "Close", &())
        .await;
}

/// `Some(true)` kept, `Some(false)` reverted, `None` no answer in time.
async fn wait_for_answer(mut signals: MessageStream, timeout: std::time::Duration) -> Option<bool> {
    let answer = async {
        while let Some(Ok(message)) = signals.next().await {
            match message.header().member().map(|m| m.as_str()) {
                Some("Kept") => return Some(true),
                Some("Reverted") => return Some(false),
                _ => {}
            }
        }
        None
    };
    tokio::time::timeout(timeout, answer).await.ok().flatten()
}

type WireMonitorChoice = (String, String, Props);
type WireLogicalConfig = (i32, i32, f64, u32, bool, Vec<WireMonitorChoice>);

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

    async fn apply_monitors_config(
        &self,
        serial: u32,
        method: u32,
        logical_monitors: Vec<WireLogicalConfig>,
        properties: Props,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<()> {
        if self.applier.is_none() {
            return Err(fdo::Error::AccessDenied("changing the monitor configuration is not allowed".into()));
        }
        let method = Method::try_from(method).map_err(fdo::Error::InvalidArgs)?;
        let (config, key, current) = {
            let layout = self.layout();
            if serial != layout.serial {
                return Err(fdo::Error::InvalidArgs(format!(
                    "stale configuration serial {serial} (current is {})",
                    layout.serial
                )));
            }
            let config = layout.parse(&logical_monitors, &properties).map_err(fdo::Error::InvalidArgs)?;
            (config, layout.key(), layout.current_config())
        };
        if method == Method::Verify {
            return Ok(());
        }
        // A change while the user is still being asked about the previous
        // one replaces it; going back still means going back to before both.
        let earlier = self.pending().take();
        if earlier.is_some() {
            close_dialog(conn).await;
        }
        self.apply_and_publish(config.clone(), &emitter)
            .await
            .map_err(fdo::Error::Failed)?;
        tracing::info!(?method, "monitor configuration applied");
        if method == Method::Persistent {
            // Nothing to keep or revert when nothing changed.
            let unchanged = earlier.is_none() && current == config;
            if self.confirm_seconds == 0 || unchanged {
                if let Err(err) = self.store.save(&key, &config) {
                    tracing::warn!(%err, "cannot remember the monitor configuration");
                }
            } else {
                let previous = earlier.map(|p| p.previous).unwrap_or(current);
                self.confirm(conn, previous, key, config).await;
            }
        }
        Ok(())
    }

    #[zbus(signal)]
    pub async fn monitors_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property)]
    async fn backlight(&self) -> (u32, Vec<Props>) {
        self.backlight_entries()
    }

    #[zbus(out_args("red", "green", "blue"))]
    async fn get_crtc_gamma(&self, serial: u32, crtc: u32) -> fdo::Result<(Vec<u16>, Vec<u16>, Vec<u16>)> {
        self.color(serial, crtc, None).await
    }

    async fn set_crtc_gamma(
        &self,
        serial: u32,
        crtc: u32,
        red: Vec<u16>,
        green: Vec<u16>,
        blue: Vec<u16>,
    ) -> fdo::Result<()> {
        self.color(serial, crtc, Some(ColorSet::Gamma((red, green, blue)))).await.map(drop)
    }

    #[zbus(name = "SetOutputCTM")]
    async fn set_output_ctm(&self, serial: u32, output: u32, ctm: (u64, u64, u64, u64, u64, u64, u64, u64, u64)) -> fdo::Result<()> {
        let (a, b, c, d, e, f, g, h, i) = ctm;
        self.color(serial, output, Some(ColorSet::Ctm([a, b, c, d, e, f, g, h, i]))).await.map(drop)
    }

    async fn set_backlight(
        &self,
        serial: u32,
        connector: &str,
        value: i32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        if serial != self.layout().serial {
            return Err(fdo::Error::InvalidArgs(format!("stale backlight serial {serial}")));
        }
        let Some((builtin, device, _)) = self.builtin_backlight().filter(|c| c.0 == connector) else {
            return Err(fdo::Error::InvalidArgs(format!("no backlight control for {connector:?}")));
        };
        if !(crate::backlight::MIN..=device.max).contains(&value) {
            return Err(fdo::Error::InvalidArgs(format!(
                "backlight value {value} outside {}..={}",
                crate::backlight::MIN,
                device.max
            )));
        }
        let backlight = self.backlight.as_ref().expect("checked above");
        backlight.set(&device, value).await.map_err(fdo::Error::Failed)?;
        tracing::debug!(connector = builtin, value, "backlight set");
        self.backlight_changed(&emitter).await?;
        Ok(())
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
        self.applier.is_some()
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
            transforms: ALL_TRANSFORMS.to_vec(),
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

    fn mode(width: i32, height: i32, current: bool) -> Mode {
        Mode { width, height, refresh_mhz: 60_000, preferred: current, current }
    }

    /// DP-1 (1920x1080 or 1280x720) left of HDMI-1 (1920x1080).
    fn two() -> Layout {
        let mut a = nested(1920, 1080);
        a.connector = "DP-1".into();
        a.modes.push(mode(1280, 720, false));
        let mut b = nested(1920, 1080);
        b.connector = "HDMI-1".into();
        b.x = 1920;
        b.primary = false;
        Layout::new(vec![a, b])
    }

    fn choice(connector: &str, width: i32, height: i32) -> MonitorChoice {
        MonitorChoice { connector: connector.into(), width, height, refresh_mhz: 60_000 }
    }

    fn logical(x: i32, y: i32, primary: bool, monitors: Vec<MonitorChoice>) -> LogicalConfig {
        LogicalConfig { x, y, scale: 1.0, transform: 0, primary, monitors }
    }

    fn side_by_side(bx: i32, by: i32) -> MonitorsConfig {
        MonitorsConfig {
            logical: vec![
                logical(0, 0, true, vec![choice("DP-1", 1920, 1080)]),
                logical(bx, by, false, vec![choice("HDMI-1", 1920, 1080)]),
            ],
        }
    }

    fn err(layout: &Layout, config: &MonitorsConfig) -> String {
        layout.check(config).expect_err("should be rejected")
    }

    #[test]
    fn scales_offered() {
        assert_eq!(supported_scales(1280, 800), [1.0, 1.25]);
        assert_eq!(supported_scales(1366, 768), [1.0]);
        assert_eq!(supported_scales(1920, 1080), [1.0, 1.25, 1.5, 2.0], "not 1.75: 1097.14 logical pixels");
        assert_eq!(supported_scales(3840, 2160), [1.0, 1.25, 1.5, 2.0, 2.5, 3.0, 3.75, 4.0]);
        assert_eq!(supported_scales(2560, 1600), [1.0, 1.25, 2.0, 2.5]);
        assert_eq!(logical_size(3840, 2160, 2.0, 1), (1080, 1920));
    }

    #[test]
    fn layout_rules() {
        let layout = two();
        assert_eq!(layout.check(&side_by_side(1920, 0)), Ok(()));
        assert_eq!(layout.check(&side_by_side(0, 1080)), Ok(()), "stacked");
        assert_eq!(layout.check(&side_by_side(1920, 500)), Ok(()), "edges partly shared");
        assert!(err(&layout, &side_by_side(1921, 0)).contains("touch"));
        assert!(err(&layout, &side_by_side(1920, 1080)).contains("touch"), "corners only");
        assert!(err(&layout, &side_by_side(1000, 0)).contains("overlap"));
        assert_eq!(layout.check(&side_by_side(-1920, 0)).unwrap_err(), "the layout must start at 0,0, not -1920,0");

        let mut both = side_by_side(1920, 0);
        both.logical[1].primary = true;
        assert!(err(&layout, &both).contains("primary"));
        both.logical[0].primary = false;
        both.logical[1].primary = false;
        assert!(err(&layout, &both).contains("primary"));

        let mut scaled = side_by_side(960, 0);
        scaled.logical[0].scale = 2.0;
        assert_eq!(layout.check(&scaled), Ok(()), "960x540 logical");
        scaled.logical[0].scale = 1.75;
        assert!(err(&layout, &scaled).contains("scale"), "1097.14 logical pixels");
        scaled.logical[0].scale = 1.3;
        assert!(err(&layout, &scaled).contains("scale"), "not a 0.25 step");
        let mut fractional = side_by_side(1280, 0);
        fractional.logical[0].scale = 1.5;
        assert_eq!(layout.check(&fractional), Ok(()), "1280x720 logical");

        let mut rotated = side_by_side(1080, 0);
        rotated.logical[0].transform = 1;
        assert_eq!(layout.check(&rotated), Ok(()), "1080x1920 portrait");
        rotated.logical[0].transform = 8;
        assert!(err(&layout, &rotated).contains("transform"));
        let mut fixed = layout.clone();
        fixed.monitors[0].transforms = vec![0];
        rotated.logical[0].transform = 1;
        assert!(err(&fixed, &rotated).contains("cannot show transform 1"));
    }

    #[test]
    fn monitors_modes_mirroring_and_turning_off() {
        let layout = two();
        let mut config = side_by_side(1920, 0);
        config.logical[1].monitors[0].connector = "DP-1".into();
        assert!(err(&layout, &config).contains("more than once"));
        config.logical[1].monitors[0].connector = "DP-2".into();
        assert!(err(&layout, &config).contains("no monitor is connected at DP-2"));
        config.logical[1].monitors[0] = choice("HDMI-1", 1024, 768);
        assert!(err(&layout, &config).contains("no 1024x768 mode"));
        assert!(err(&layout, &MonitorsConfig::default()).contains("stay on"));

        let mirror = MonitorsConfig {
            logical: vec![logical(0, 0, true, vec![choice("DP-1", 1920, 1080), choice("HDMI-1", 1920, 1080)])],
        };
        assert_eq!(layout.check(&mirror), Ok(()));
        let mut uneven = mirror.clone();
        uneven.logical[0].monitors[0] = choice("DP-1", 1280, 720);
        assert!(err(&layout, &uneven).contains("same size"));
        let mirrored = Layout::new(layout.applied(&mirror));
        let (_, _, logical_monitors, _) = mirrored.current_state();
        assert_eq!(logical_monitors.len(), 1, "one logical monitor shows both");
        assert_eq!(logical_monitors[0].5.len(), 2);

        let only_dp = MonitorsConfig { logical: vec![logical(0, 0, true, vec![choice("DP-1", 1280, 720)])] };
        assert_eq!(layout.check(&only_dp), Ok(()));
        let applied = layout.applied(&only_dp);
        assert!(applied[1].modes.iter().all(|m| !m.current), "HDMI-1 is off");
        assert!(applied[0].modes[1].current, "DP-1 at 1280x720");
        let off = Layout::new(applied);
        assert_eq!(off.current_state().2.len(), 1);
        assert!(off.is_current(&only_dp));
        assert!(!layout.is_current(&only_dp));
    }

    #[test]
    fn current_config_round_trips() {
        let layout = two();
        assert!(layout.is_current(&layout.current_config()));
        let mirror = MonitorsConfig {
            logical: vec![logical(0, 0, true, vec![choice("DP-1", 1920, 1080), choice("HDMI-1", 1920, 1080)])],
        };
        let mirrored = Layout::new(layout.applied(&mirror));
        assert_eq!(mirrored.current_config(), mirror);
    }

    #[test]
    fn parse_wire_arguments() {
        let layout = two();
        let wire = |mode: &str| -> Vec<WireLogicalConfig> {
            vec![
                (0, 0, 1.0, 0, true, vec![("DP-1".into(), mode.into(), Props::new())]),
                (1920, 0, 1.0, 0, false, vec![("HDMI-1".into(), "1920x1080@60.000".into(), Props::new())]),
            ]
        };
        let config = layout.parse(&wire("1920x1080@60.000"), &Props::new()).unwrap();
        assert_eq!(config, side_by_side(1920, 0));
        assert!(layout.parse(&wire("800x600@60.000"), &Props::new()).unwrap_err().contains("no mode"));
        let hdr = vec![(0, 0, 1.0, 0, true, vec![("DP-1".to_owned(), "1920x1080@60.000".to_owned(), props([("color-mode", OwnedValue::from(1u32))]))])];
        assert!(layout.parse(&hdr, &Props::new()).unwrap_err().contains("color mode"));
        let sdr = vec![(0, 0, 1.0, 0, true, vec![("DP-1".to_owned(), "1920x1080@60.000".to_owned(), props([("color-mode", OwnedValue::from(0u32))]))])];
        assert!(layout.parse(&sdr, &Props::new()).is_ok());
        let physical = props([("layout-mode", OwnedValue::from(2u32))]);
        assert!(layout.parse(&wire("1920x1080@60.000"), &physical).unwrap_err().contains("layout mode 2"));
        assert_eq!(Method::try_from(2), Ok(Method::Persistent));
        assert!(Method::try_from(3).is_err());
    }

    #[test]
    fn store_remembers_per_monitor_set() {
        let dir = std::env::temp_dir().join(format!("helix-monitors-{}", std::process::id()));
        let store = Store::at(dir.join("sub").join("monitors.toml"));
        let layout = two();
        assert_eq!(store.load(&layout.key()), None);
        store.save(&layout.key(), &side_by_side(1920, 0)).unwrap();
        store.save("other", &side_by_side(0, 1080)).unwrap();
        store.save(&layout.key(), &side_by_side(1920, 500)).unwrap();
        assert_eq!(store.load(&layout.key()), Some(side_by_side(1920, 500)));
        assert_eq!(store.load("other"), Some(side_by_side(0, 1080)));
        let reversed = Layout::new(layout.monitors.iter().rev().cloned().collect());
        assert_eq!(reversed.key(), layout.key(), "order doesn't matter");
        let _ = std::fs::remove_dir_all(&dir);
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
