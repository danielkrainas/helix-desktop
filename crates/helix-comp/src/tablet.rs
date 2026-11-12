//! Touchscreen and drawing tablet settings, and where their input lands
//! (helix-desktop-spec `contracts/gsettings/input-settings.md`,
//! "Touchscreens and tablets").

use gio::prelude::*;

pub const TABLET: &str = "org.gnome.desktop.peripherals.tablet";
pub const STYLUS: &str = "org.gnome.desktop.peripherals.tablet.stylus";
pub const TOUCHSCREEN: &str = "org.gnome.desktop.peripherals.touchscreen";
const BASE: &str = "/org/gnome/desktop/peripherals";

/// Where a device's relocatable settings live: `<BASE>/<dir>/<vid>:<pid>/`
/// (USB ids as four lower-case hex digits).
pub fn device_path(dir: &str, usb: (u32, u32)) -> String {
    format!("{BASE}/{dir}/{:04x}:{:04x}/", usb.0, usb.1)
}

/// Where a stylus's settings live: by its serial number, or for styli
/// without one, by the tablet.
pub fn stylus_path(serial: u64, usb: Option<(u32, u32)>) -> String {
    match (serial, usb) {
        (0, Some((v, p))) => format!("{BASE}/stylus/default-{v:04x}:{p:04x}/"),
        (0, None) => format!("{BASE}/stylus/default/"),
        (s, _) => format!("{BASE}/stylus/{s:x}/"),
    }
}

fn relocatable(schema: &str, path: &str) -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    let schema = source.lookup(schema, true)?;
    Some(gio::Settings::new_full(&schema, None::<&gio::SettingsBackend>, Some(path)))
}

/// A monitor as the settings name it: [vendor, product, serial]; all
/// empty: chosen automatically.
pub fn monitor_spec(value: Vec<String>) -> Option<Vec<String>> {
    (value.len() >= 3 && value.iter().take(3).any(|s| !s.is_empty())).then_some(value)
}

/// A touchscreen's `output`.
pub fn touchscreen_output(usb: Option<(u32, u32)>) -> Option<Vec<String>> {
    let settings = relocatable(TOUCHSCREEN, &device_path("touchscreens", usb?))?;
    monitor_spec(settings.strv("output").iter().map(|s| s.to_string()).collect())
}

/// A tablet's settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Tablet {
    /// Dead margins: left, right, top, bottom, as fractions of the tablet.
    pub area: [f64; 4],
    pub keep_aspect: bool,
    pub left_handed: bool,
    /// `mapping` `relative`: the pen moves the pointer like a mouse.
    pub relative: bool,
    pub output: Option<Vec<String>>,
}

impl Default for Tablet {
    fn default() -> Self {
        Self { area: [0.0; 4], keep_aspect: false, left_handed: false, relative: false, output: None }
    }
}

/// `area` values as fractions: the schema says percentages, GNOME's
/// tools write fractions; values above 1 are taken as percentages.
pub fn area_fractions(values: &[f64]) -> [f64; 4] {
    let mut out = [0.0; 4];
    for (o, v) in out.iter_mut().zip(values) {
        let f = if *v > 1.0 { v / 100.0 } else { *v };
        *o = f.clamp(0.0, 0.45);
    }
    out
}

pub fn read_tablet(usb: Option<(u32, u32)>) -> Tablet {
    let Some(settings) = usb.and_then(|usb| relocatable(TABLET, &device_path("tablets", usb))) else {
        return Tablet::default();
    };
    let area: Vec<f64> = settings.value("area").get().unwrap_or_default();
    Tablet {
        area: area_fractions(&area),
        keep_aspect: settings.boolean("keep-aspect"),
        left_handed: settings.boolean("left-handed"),
        relative: settings.string("mapping") == "relative",
        output: monitor_spec(settings.strv("output").iter().map(|s| s.to_string()).collect()),
    }
}

/// What a stylus button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonAction {
    /// A pointer button (evdev code).
    Button(u32),
    /// Map the tablet to the next monitor.
    SwitchMonitor,
    /// Nothing (Helix does not send key combinations for a stylus
    /// button's `keybinding`).
    Nothing,
}

const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;
/// The stylus buttons libinput reports: first, second, third.
pub const BTN_STYLUS: u32 = 0x14b;
pub const BTN_STYLUS2: u32 = 0x14c;
pub const BTN_STYLUS3: u32 = 0x149;

/// A `*-button-action` value for stylus button `index` (0–2); `default`
/// is middle, right and back for the three buttons.
pub fn button_action(value: &str, index: usize) -> ButtonAction {
    match value {
        "middle" => ButtonAction::Button(BTN_MIDDLE),
        "right" => ButtonAction::Button(BTN_RIGHT),
        "back" => ButtonAction::Button(BTN_SIDE),
        "forward" => ButtonAction::Button(BTN_EXTRA),
        "switch-monitor" => ButtonAction::SwitchMonitor,
        "keybinding" => ButtonAction::Nothing,
        _ => ButtonAction::Button([BTN_MIDDLE, BTN_RIGHT, BTN_SIDE][index.min(2)]),
    }
}

/// A stylus's button actions, by libinput button code.
pub fn read_stylus(serial: u64, usb: Option<(u32, u32)>) -> [(u32, ButtonAction); 3] {
    read_stylus_settings(serial, usb).buttons
}

/// How physical pressure becomes the pressure apps get: `range` (percent
/// of the physical range that maps to the whole logical range) and `curve`
/// (the two inner control points of a Bézier curve from (0, 0) to
/// (100, 100)).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PressureCurve {
    pub curve: [f64; 4],
    pub range: [f64; 2],
}

impl Default for PressureCurve {
    fn default() -> Self {
        Self { curve: [0.0, 0.0, 100.0, 100.0], range: [0.0, 100.0] }
    }
}

impl PressureCurve {
    /// From the settings' integer arrays; anything malformed is the
    /// default (a straight line over the whole range).
    pub fn from_settings(curve: &[i32], range: &[i32]) -> Self {
        let mut out = Self::default();
        if let [x1, y1, x2, y2] = curve {
            let c = [*x1, *y1, *x2, *y2].map(|v| f64::from(v.clamp(0, 100)));
            out.curve = c;
        }
        if let [lo, hi] = range {
            let (lo, hi) = (f64::from((*lo).clamp(0, 100)), f64::from((*hi).clamp(0, 100)));
            if hi > lo {
                out.range = [lo, hi];
            }
        }
        out
    }

    /// The logical pressure (0–1) for a physical one (0–1).
    pub fn apply(&self, raw: f64) -> f64 {
        let [lo, hi] = self.range;
        let p = ((raw.clamp(0.0, 1.0) * 100.0 - lo) / (hi - lo)).clamp(0.0, 1.0);
        let [x1, y1, x2, y2] = self.curve.map(|v| v / 100.0);
        if x1 == y1 && x2 == y2 {
            return p;
        }
        // The Bézier curve's x is increasing in t: find t for x = p.
        let bezier = |t: f64, a: f64, b: f64| 3.0 * (1.0 - t) * (1.0 - t) * t * a + 3.0 * (1.0 - t) * t * t * b + t * t * t;
        if p <= 0.0 || p >= 1.0 {
            return p;
        }
        let (mut low, mut high) = (0.0f64, 1.0f64);
        for _ in 0..40 {
            let mid = (low + high) / 2.0;
            if bezier(mid, x1, x2) < p {
                low = mid;
            } else {
                high = mid;
            }
        }
        bezier((low + high) / 2.0, y1, y2).clamp(0.0, 1.0)
    }
}

/// A stylus's settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Stylus {
    pub buttons: [(u32, ButtonAction); 3],
    pub pressure: PressureCurve,
    /// For the eraser end.
    pub eraser_pressure: PressureCurve,
}

pub fn read_stylus_settings(serial: u64, usb: Option<(u32, u32)>) -> Stylus {
    let keys = ["button-action", "secondary-button-action", "tertiary-button-action"];
    let defaults = ["right", "middle", "default"];
    let settings = relocatable(STYLUS, &stylus_path(serial, usb));
    let codes = [BTN_STYLUS, BTN_STYLUS2, BTN_STYLUS3];
    let buttons = std::array::from_fn(|i| {
        let value = settings.as_ref().map_or_else(|| defaults[i].to_owned(), |s| s.string(keys[i]).to_string());
        (codes[i], button_action(&value, i))
    });
    let curve = |curve: &str, range: &str| {
        settings.as_ref().map_or_else(PressureCurve::default, |s| {
            let c: Vec<i32> = s.value(curve).get().unwrap_or_default();
            let r: Vec<i32> = s.value(range).get().unwrap_or_default();
            PressureCurve::from_settings(&c, &r)
        })
    };
    Stylus { buttons, pressure: curve("pressure-curve", "pressure-range"), eraser_pressure: curve("eraser-pressure-curve", "eraser-pressure-range") }
}

pub const PAD_BUTTON: &str = "org.gnome.desktop.peripherals.tablet.pad-button";

/// What a pad button does (`pad-button` `action`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PadAction {
    /// `none`: the button goes to the app (`zwp_tablet_pad_v2`).
    App,
    /// Show what the pad's buttons do.
    Help,
    /// Map the tablet to the next monitor.
    SwitchMonitor,
    /// Type this key combination (an accelerator such as `<Control>z`).
    Keybinding(String),
}

pub fn pad_action(action: &str, keybinding: &str) -> PadAction {
    match action {
        "help" => PadAction::Help,
        "switch-monitor" => PadAction::SwitchMonitor,
        "keybinding" if !keybinding.trim().is_empty() => PadAction::Keybinding(keybinding.trim().to_owned()),
        _ => PadAction::App,
    }
}

/// A pad button's name: A, B, C… (then 27, 28… past Z).
pub fn pad_button_name(button: u32) -> String {
    char::from_u32(u32::from(b'A') + button).filter(|c| c.is_ascii_uppercase()).map_or_else(|| (button + 1).to_string(), String::from)
}

/// Where pad button `button`'s settings live: below the tablet's, as
/// `button<name>/`.
pub fn pad_button_path(usb: (u32, u32), button: u32) -> String {
    format!("{}button{}/", device_path("tablets", usb), pad_button_name(button))
}

pub fn read_pad_button(usb: Option<(u32, u32)>, button: u32) -> PadAction {
    let Some(settings) = usb.and_then(|usb| relocatable(PAD_BUTTON, &pad_button_path(usb, button))) else { return PadAction::App };
    pad_action(&settings.string("action"), &settings.string("keybinding"))
}

/// The text "help" shows: each button with what it does.
pub fn pad_help(actions: &[(u32, PadAction)]) -> String {
    actions
        .iter()
        .map(|(button, action)| {
            let what = match action {
                PadAction::App => "Application defined".to_owned(),
                PadAction::Help => "Show this help".to_owned(),
                PadAction::SwitchMonitor => "Switch monitor".to_owned(),
                PadAction::Keybinding(keys) => format!("Send keys {keys}"),
            };
            format!("Button {}: {what}", pad_button_name(*button))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A strip's position (0–1) as `zwp_tablet_pad_strip_v2` gives it (0–65535).
pub fn strip_value(position: f64) -> u32 {
    (position.clamp(0.0, 1.0) * 65535.0).round() as u32
}

/// Where on a monitor of `size` a pen at `pos` (fractions of the tablet's
/// width and height) points: dead margins cut, left-handed turned around,
/// and with `keep_aspect` the tablet's area cut to the monitor's shape
/// (from its top-left; `tablet_mm` is its size). `None` outside the area.
pub fn map_point(pos: (f64, f64), tablet: &Tablet, tablet_mm: Option<(f64, f64)>, size: (f64, f64)) -> Option<(f64, f64)> {
    let (mut x, mut y) = pos;
    if tablet.left_handed {
        (x, y) = (1.0 - x, 1.0 - y);
    }
    let [l, r, t, b] = tablet.area;
    let (aw, ah) = (1.0 - l - r, 1.0 - t - b);
    let (mut fx, mut fy) = ((x - l) / aw, (y - t) / ah);
    if tablet.keep_aspect {
        if let Some((mw, mh)) = tablet_mm.filter(|(w, h)| *w > 0.0 && *h > 0.0) {
            let tablet_aspect = (mw * aw) / (mh * ah);
            let monitor_aspect = size.0 / size.1;
            if tablet_aspect > monitor_aspect {
                fx /= monitor_aspect / tablet_aspect;
            } else {
                fy /= tablet_aspect / monitor_aspect;
            }
        }
    }
    let inside = (-0.001..=1.001).contains(&fx) && (-0.001..=1.001).contains(&fy);
    inside.then(|| (fx.clamp(0.0, 1.0) * size.0, fy.clamp(0.0, 1.0) * size.1))
}

/// How far the pointer moves per millimetre of pen motion with relative
/// mapping (logical pixels).
pub const RELATIVE_PX_PER_MM: f64 = 4.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(device_path("tablets", (0x56a, 0x357)), "/org/gnome/desktop/peripherals/tablets/056a:0357/");
        assert_eq!(stylus_path(0x99800b2, None), "/org/gnome/desktop/peripherals/stylus/99800b2/");
        assert_eq!(stylus_path(0, Some((0x56a, 0x357))), "/org/gnome/desktop/peripherals/stylus/default-056a:0357/");
        assert_eq!(monitor_spec(vec!["".into(), "".into(), "".into()]), None);
        assert!(monitor_spec(vec!["DEL".into(), "".into(), "".into()]).is_some());
        assert_eq!(area_fractions(&[0.1, 10.0, 0.0, 0.9]), [0.1, 0.1, 0.0, 0.45]);
    }

    #[test]
    fn buttons() {
        assert_eq!(button_action("default", 0), ButtonAction::Button(BTN_MIDDLE));
        assert_eq!(button_action("default", 2), ButtonAction::Button(BTN_SIDE));
        assert_eq!(button_action("right", 1), ButtonAction::Button(BTN_RIGHT));
        assert_eq!(button_action("switch-monitor", 0), ButtonAction::SwitchMonitor);
        assert_eq!(button_action("keybinding", 0), ButtonAction::Nothing);
    }

    #[test]
    fn mapping() {
        let size = (1920.0, 1080.0);
        let plain = Tablet::default();
        assert_eq!(map_point((0.5, 0.5), &plain, None, size), Some((960.0, 540.0)));
        let lefty = Tablet { left_handed: true, ..Tablet::default() };
        assert_eq!(map_point((0.0, 0.0), &lefty, None, size), Some((1920.0, 1080.0)));
        // A 10 % margin on every side: the margin is outside.
        let margins = Tablet { area: [0.1; 4], ..Tablet::default() };
        assert_eq!(map_point((0.1, 0.9), &margins, None, size), Some((0.0, 1080.0)));
        assert_eq!(map_point((0.05, 0.5), &margins, None, size), None);
        // A square tablet on a 16:9 monitor keeps its top 9/16.
        let aspect = Tablet { keep_aspect: true, ..Tablet::default() };
        let (_, y) = map_point((1.0, 0.5625), &aspect, Some((100.0, 100.0)), size).unwrap();
        assert!((y - 1080.0).abs() < 1e-6);
        assert_eq!(map_point((0.5, 0.9), &aspect, Some((100.0, 100.0)), size), None);
    }

    #[test]
    fn pressure_curves() {
        let straight = PressureCurve::default();
        assert_eq!(straight.apply(0.3), 0.3);
        let ranged = PressureCurve::from_settings(&[0, 0, 100, 100], &[20, 70]);
        assert_eq!((ranged.apply(0.1), ranged.apply(0.45), ranged.apply(0.9)), (0.0, 0.5, 1.0));
        let soft = PressureCurve::from_settings(&[0, 50, 50, 100], &[0, 100]);
        assert!(soft.apply(0.25) > 0.25 && soft.apply(0.25) < 1.0, "a soft curve rises early: {}", soft.apply(0.25));
        assert!(soft.apply(0.5) > 0.5);
        let samples: Vec<f64> = (0..=20).map(|i| soft.apply(f64::from(i) / 20.0)).collect();
        assert!(samples.windows(2).all(|w| w[0] <= w[1]), "never decreases");
        let firm = PressureCurve::from_settings(&[50, 0, 100, 50], &[0, 100]);
        assert!(firm.apply(0.5) < 0.5);
        assert_eq!((soft.apply(0.0), soft.apply(1.0)), (0.0, 1.0));
        assert_eq!(PressureCurve::from_settings(&[1, 2], &[80, 10]), PressureCurve::default(), "malformed values");
    }

    #[test]
    fn pad_buttons() {
        assert_eq!(pad_action("none", "<Control>z"), PadAction::App);
        assert_eq!(pad_action("keybinding", " <Control>z "), PadAction::Keybinding("<Control>z".into()));
        assert_eq!(pad_action("keybinding", ""), PadAction::App);
        assert_eq!((pad_action("help", ""), pad_action("switch-monitor", "")), (PadAction::Help, PadAction::SwitchMonitor));
        assert_eq!((pad_button_name(0), pad_button_name(25), pad_button_name(26)), ("A".into(), "Z".into(), "27".into()));
        assert_eq!(pad_button_path((0x56a, 0x357), 2), "/org/gnome/desktop/peripherals/tablets/056a:0357/buttonC/");
        let help = pad_help(&[(0, PadAction::Keybinding("<Control>z".into())), (1, PadAction::App)]);
        assert_eq!(help, "Button A: Send keys <Control>z\nButton B: Application defined");
        assert_eq!((strip_value(0.0), strip_value(0.5), strip_value(2.0)), (0, 32768, 65535));
    }
}
