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
    /// Nothing (Helix does not send key combinations for `keybinding`).
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
    let keys = ["button-action", "secondary-button-action", "tertiary-button-action"];
    let defaults = ["right", "middle", "default"];
    let settings = relocatable(STYLUS, &stylus_path(serial, usb));
    let codes = [BTN_STYLUS, BTN_STYLUS2, BTN_STYLUS3];
    std::array::from_fn(|i| {
        let value = settings.as_ref().map_or_else(|| defaults[i].to_owned(), |s| s.string(keys[i]).to_string());
        (codes[i], button_action(&value, i))
    })
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
}
