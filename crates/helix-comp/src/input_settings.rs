//! Keyboard and pointing device settings from GSettings, as the compositor
//! applies them (helix-desktop-spec `contracts/gsettings/input-settings.md`).

use gio::prelude::*;

pub const INPUT_SOURCES: &str = "org.gnome.desktop.input-sources";
pub const KEYBOARD: &str = "org.gnome.desktop.peripherals.keyboard";
pub const MOUSE: &str = "org.gnome.desktop.peripherals.mouse";
pub const TOUCHPAD: &str = "org.gnome.desktop.peripherals.touchpad";
pub const TRACKBALL: &str = "org.gnome.desktop.peripherals.trackball";
pub const POINTINGSTICK: &str = "org.gnome.desktop.peripherals.pointingstick";
/// Every schema the compositor follows.
pub const SCHEMAS: [&str; 6] = [INPUT_SOURCES, KEYBOARD, MOUSE, TOUCHPAD, TRACKBALL, POINTINGSTICK];

/// Points of a custom acceleration function: (step, speeds).
#[derive(Debug, Clone, PartialEq)]
pub struct AccelCurve {
    pub step: f64,
    pub speeds: Vec<f64>,
}

/// `custom-accel-config`: curves for pointer motion and scrolling.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CustomAccel {
    pub pointer: Option<AccelCurve>,
    pub scroll: Option<AccelCurve>,
}

impl CustomAccel {
    /// From the `a{sv}` value of `custom-accel-config`.
    pub fn from_dict(dict: &std::collections::HashMap<String, gio::glib::Variant>) -> Self {
        let curve = |prefix: &str| {
            let step = dict.get(&format!("{prefix}-step"))?.get::<f64>()?;
            let speeds = dict.get(&format!("{prefix}-speeds"))?.get::<Vec<f64>>()?;
            (step > 0.0 && speeds.len() >= 2).then_some(AccelCurve { step, speeds })
        };
        Self { pointer: curve("pointer"), scroll: curve("scroll") }
    }
}

/// An X button number (as in `scroll-wheel-emulation-button`) as an evdev
/// button code; `None` for 0 (off) and numbers without a button.
pub fn x_button_to_evdev(button: i32) -> Option<u32> {
    const BTN_LEFT: u32 = 0x110;
    const BTN_RIGHT: u32 = 0x111;
    const BTN_MIDDLE: u32 = 0x112;
    const BTN_SIDE: u32 = 0x113;
    match button {
        1 => Some(BTN_LEFT),
        2 => Some(BTN_MIDDLE),
        3 => Some(BTN_RIGHT),
        // 4–7 are the wheel in X.
        8..=24 => Some(BTN_SIDE + (button as u32 - 8)),
        _ => None,
    }
}

/// An input source: (type, id), e.g. ("xkb", "de+nodeadkeys").
pub type Source = (String, String);

/// The XKB keymap for the configured layouts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    pub model: String,
    /// Comma-separated layouts, one per source.
    pub layout: String,
    /// Comma-separated variants, matching `layout`.
    pub variant: String,
    pub options: String,
    /// Index of the active layout.
    pub active: u32,
}

/// The active source: the first most-recently-used one still configured,
/// else the first.
pub fn active_source(sources: &[Source], mru: &[Source]) -> Option<usize> {
    if sources.is_empty() {
        return None;
    }
    Some(mru.iter().find_map(|m| sources.iter().position(|s| s == m)).unwrap_or(0))
}

/// Build the keymap from the `xkb` sources; `None` keeps the default.
pub fn keymap(sources: &[Source], mru: &[Source], model: &str, options: &[String]) -> Option<Keymap> {
    let xkb: Vec<Source> = sources.iter().filter(|(t, _)| t == "xkb").cloned().collect();
    let active = active_source(&xkb, mru)?;
    let (layouts, variants): (Vec<&str>, Vec<&str>) = xkb
        .iter()
        .map(|(_, id)| id.split_once('+').unwrap_or((id.as_str(), "")))
        .unzip();
    Some(Keymap {
        model: model.to_owned(),
        layout: layouts.join(","),
        variant: variants.join(","),
        options: options.join(","),
        active: active as u32,
    })
}

/// Key repeat as (rate per second, delay in ms); rate 0 turns it off.
pub fn repeat(enabled: bool, delay: u32, interval: u32) -> (i32, i32) {
    let rate = if enabled { (1000 / interval.max(1)).max(1) as i32 } else { 0 };
    (rate, delay.min(i32::MAX as u32) as i32)
}

/// Settings shared by pointing devices.
#[derive(Debug, Clone, PartialEq)]
pub struct Pointer {
    pub speed: f64,
    /// `flat`, `adaptive`, `custom`, or anything else for the device default.
    pub accel_profile: String,
    pub custom_accel: CustomAccel,
    pub natural_scroll: bool,
    pub left_handed: bool,
    pub middle_click_emulation: bool,
    /// X button that scrolls while held (0: off).
    pub scroll_button: i32,
    pub scroll_button_lock: bool,
}

impl Default for Pointer {
    fn default() -> Self {
        Self {
            speed: 0.0,
            accel_profile: "default".into(),
            custom_accel: CustomAccel::default(),
            natural_scroll: false,
            left_handed: false,
            middle_click_emulation: false,
            scroll_button: 0,
            scroll_button_lock: false,
        }
    }
}

/// A pointing stick.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PointingStick {
    pub pointer: Pointer,
    /// `on-button-down`, `none`, or anything else for the device default.
    pub scroll_method: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Touchpad {
    pub pointer: Pointer,
    pub tap_to_click: bool,
    pub tap_and_drag: bool,
    pub tap_and_drag_lock: bool,
    /// `lrm`, `lmr`, or `default`.
    pub tap_button_map: String,
    pub two_finger_scrolling: bool,
    pub edge_scrolling: bool,
    pub disable_while_typing: bool,
    /// `none`, `areas`, `fingers`, or `default`.
    pub click_method: String,
    /// `enabled`, `disabled` or `disabled-on-external-mouse`.
    pub send_events: String,
    /// Multiplies finger scrolling.
    pub scroll_speed: f64,
}

impl Default for Touchpad {
    fn default() -> Self {
        Self {
            pointer: Pointer {
                natural_scroll: true,
                ..Pointer::default()
            },
            tap_to_click: true,
            tap_and_drag: true,
            tap_and_drag_lock: false,
            tap_button_map: "default".into(),
            two_finger_scrolling: true,
            edge_scrolling: false,
            disable_while_typing: true,
            click_method: "fingers".into(),
            send_events: "enabled".into(),
            scroll_speed: 1.0,
        }
    }
}

/// Everything the compositor applies.
#[derive(Debug, Clone, PartialEq)]
pub struct InputSettings {
    pub keymap: Option<Keymap>,
    /// (rate per second, delay ms)
    pub repeat: (i32, i32),
    pub mouse: Pointer,
    pub touchpad: Touchpad,
    pub trackball: Pointer,
    pub pointing_stick: PointingStick,
}

impl Default for InputSettings {
    fn default() -> Self {
        Self {
            keymap: None,
            repeat: repeat(true, 500, 30),
            mouse: Pointer::default(),
            touchpad: Touchpad::default(),
            trackball: Pointer::default(),
            pointing_stick: PointingStick::default(),
        }
    }
}

fn settings(schema: &str) -> Option<gio::Settings> {
    helix_settings::has_schema(schema).then(|| gio::Settings::new(schema))
}

/// Keys of one schema, with defaults for keys older schemas lack.
struct Keys {
    settings: gio::Settings,
    schema: gio::SettingsSchema,
}

impl Keys {
    fn open(schema: &str) -> Option<Self> {
        let found = gio::SettingsSchemaSource::default()?.lookup(schema, true)?;
        Some(Self { settings: gio::Settings::new(schema), schema: found })
    }
    fn has(&self, key: &str) -> bool {
        self.schema.has_key(key)
    }
    fn double(&self, key: &str, default: f64) -> f64 {
        if self.has(key) { self.settings.double(key) } else { default }
    }
    fn boolean(&self, key: &str, default: bool) -> bool {
        if self.has(key) { self.settings.boolean(key) } else { default }
    }
    fn int(&self, key: &str, default: i32) -> i32 {
        if self.has(key) { self.settings.int(key) } else { default }
    }
    fn string(&self, key: &str, default: &str) -> String {
        if self.has(key) { self.settings.string(key).to_string() } else { default.to_owned() }
    }
    fn accel(&self) -> CustomAccel {
        if !self.has("custom-accel-config") {
            return CustomAccel::default();
        }
        let dict = self.settings.value("custom-accel-config").get::<std::collections::HashMap<String, gio::glib::Variant>>();
        dict.map(|d| CustomAccel::from_dict(&d)).unwrap_or_default()
    }
}

fn sources(settings: &gio::Settings, key: &str) -> Vec<Source> {
    settings.value(key).get::<Vec<(String, String)>>().unwrap_or_default()
}

/// Read the current settings (defaults for schemas that are not installed).
pub fn read() -> InputSettings {
    let mut out = InputSettings::default();
    if let Some(s) = settings(INPUT_SOURCES) {
        let options: Vec<String> = s.strv("xkb-options").iter().map(|o| o.to_string()).collect();
        out.keymap = keymap(&sources(&s, "sources"), &sources(&s, "mru-sources"), &s.string("xkb-model"), &options);
    }
    if let Some(s) = settings(KEYBOARD) {
        out.repeat = repeat(s.boolean("repeat"), s.uint("delay"), s.uint("repeat-interval"));
    }
    if let Some(s) = Keys::open(MOUSE) {
        out.mouse = Pointer {
            speed: s.double("speed", 0.0),
            accel_profile: s.string("accel-profile", "default"),
            custom_accel: s.accel(),
            natural_scroll: s.boolean("natural-scroll", false),
            left_handed: s.boolean("left-handed", false),
            middle_click_emulation: s.boolean("middle-click-emulation", false),
            scroll_button: s.int("scroll-wheel-emulation-button", 0),
            scroll_button_lock: s.boolean("scroll-wheel-emulation-button-lock", false),
        };
    }
    // Trackballs and pointing sticks take what their schemas lack from the
    // mouse.
    let mouse = out.mouse.clone();
    if let Some(s) = Keys::open(TRACKBALL) {
        out.trackball = Pointer {
            accel_profile: s.string("accel-profile", "default"),
            custom_accel: s.accel(),
            middle_click_emulation: s.boolean("middle-click-emulation", false),
            scroll_button: s.int("scroll-wheel-emulation-button", 0),
            scroll_button_lock: s.boolean("scroll-wheel-emulation-button-lock", false),
            ..mouse.clone()
        };
    } else {
        out.trackball = mouse.clone();
    }
    out.pointing_stick = match Keys::open(POINTINGSTICK) {
        Some(s) => PointingStick {
            pointer: Pointer {
                speed: s.double("speed", 0.0),
                accel_profile: s.string("accel-profile", "default"),
                custom_accel: s.accel(),
                scroll_button: 0,
                scroll_button_lock: false,
                ..mouse.clone()
            },
            scroll_method: s.string("scroll-method", "default"),
        },
        None => PointingStick { pointer: Pointer { scroll_button: 0, ..mouse.clone() }, scroll_method: "default".into() },
    };
    if let Some(s) = settings(TOUCHPAD) {
        let keys = Keys::open(TOUCHPAD).expect("installed");
        let left_handed = match s.string("left-handed").as_str() {
            "left" => true,
            "right" => false,
            _ => out.mouse.left_handed,
        };
        out.touchpad = Touchpad {
            pointer: Pointer {
                speed: s.double("speed"),
                accel_profile: s.string("accel-profile").to_string(),
                custom_accel: keys.accel(),
                natural_scroll: s.boolean("natural-scroll"),
                left_handed,
                middle_click_emulation: s.boolean("middle-click-emulation"),
                scroll_button: 0,
                scroll_button_lock: false,
            },
            scroll_speed: keys.double("scroll-speed", 1.0).clamp(0.01, 100.0),
            tap_to_click: s.boolean("tap-to-click"),
            tap_and_drag: s.boolean("tap-and-drag"),
            tap_and_drag_lock: s.boolean("tap-and-drag-lock"),
            tap_button_map: s.string("tap-button-map").to_string(),
            two_finger_scrolling: s.boolean("two-finger-scrolling-enabled"),
            edge_scrolling: s.boolean("edge-scrolling-enabled"),
            disable_while_typing: s.boolean("disable-while-typing"),
            click_method: s.string("click-method").to_string(),
            send_events: s.string("send-events").to_string(),
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(t: &str, id: &str) -> Source {
        (t.into(), id.into())
    }

    #[test]
    fn keymap_from_sources() {
        assert_eq!(keymap(&[], &[], "pc105", &[]), None, "no sources: keep the default");
        assert_eq!(keymap(&[src("ibus", "anthy")], &[], "pc105", &[]), None);

        let sources = [src("xkb", "us"), src("ibus", "anthy"), src("xkb", "de+nodeadkeys")];
        let map = keymap(&sources, &[], "pc105", &["caps:escape".into(), "compose:ralt".into()]).unwrap();
        assert_eq!(map.layout, "us,de");
        assert_eq!(map.variant, ",nodeadkeys");
        assert_eq!(map.options, "caps:escape,compose:ralt");
        assert_eq!(map.active, 0);

        let mru = [src("xkb", "fr"), src("xkb", "de+nodeadkeys"), src("xkb", "us")];
        assert_eq!(keymap(&sources, &mru, "pc105", &[]).unwrap().active, 1, "first MRU entry still configured");
    }

    #[test]
    fn buttons_and_curves() {
        assert_eq!(x_button_to_evdev(0), None);
        assert_eq!(x_button_to_evdev(1), Some(0x110));
        assert_eq!(x_button_to_evdev(2), Some(0x112), "middle");
        assert_eq!(x_button_to_evdev(3), Some(0x111), "right");
        assert_eq!(x_button_to_evdev(5), None, "the wheel");
        assert_eq!(x_button_to_evdev(8), Some(0x113), "back");
        assert_eq!(x_button_to_evdev(9), Some(0x114), "forward");
        let v = |x: gio::glib::Variant| x;
        let dict = std::collections::HashMap::from([
            ("pointer-step".to_owned(), v(2.0f64.into())),
            ("pointer-speeds".to_owned(), v(vec![0.0f64, 1.0, 3.0].into())),
            ("scroll-step".to_owned(), v(0.0f64.into())),
            ("scroll-speeds".to_owned(), v(vec![0.0f64, 1.0].into())),
        ]);
        let accel = CustomAccel::from_dict(&dict);
        assert_eq!(accel.pointer, Some(AccelCurve { step: 2.0, speeds: vec![0.0, 1.0, 3.0] }));
        assert_eq!(accel.scroll, None, "a step of 0 is not usable");
    }

    #[test]
    fn repeat_values() {
        assert_eq!(repeat(true, 500, 30), (33, 500));
        assert_eq!(repeat(false, 500, 30), (0, 500));
        assert_eq!(repeat(true, 250, 0), (1000, 250));
    }
}
