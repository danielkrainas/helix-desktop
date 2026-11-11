//! Keyboard, mouse and touchpad settings from GSettings, as the compositor
//! applies them (helix-desktop-spec `contracts/gsettings/input-settings.md`).

use gio::prelude::*;

pub const INPUT_SOURCES: &str = "org.gnome.desktop.input-sources";
pub const KEYBOARD: &str = "org.gnome.desktop.peripherals.keyboard";
pub const MOUSE: &str = "org.gnome.desktop.peripherals.mouse";
pub const TOUCHPAD: &str = "org.gnome.desktop.peripherals.touchpad";
/// Every schema the compositor follows.
pub const SCHEMAS: [&str; 4] = [INPUT_SOURCES, KEYBOARD, MOUSE, TOUCHPAD];

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

/// Settings shared by mice and touchpads.
#[derive(Debug, Clone, PartialEq)]
pub struct Pointer {
    pub speed: f64,
    /// `flat`, `adaptive`, or anything else for the device default.
    pub accel_profile: String,
    pub natural_scroll: bool,
    pub left_handed: bool,
    pub middle_click_emulation: bool,
}

impl Default for Pointer {
    fn default() -> Self {
        Self {
            speed: 0.0,
            accel_profile: "default".into(),
            natural_scroll: false,
            left_handed: false,
            middle_click_emulation: false,
        }
    }
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
}

impl Default for InputSettings {
    fn default() -> Self {
        Self {
            keymap: None,
            repeat: repeat(true, 500, 30),
            mouse: Pointer::default(),
            touchpad: Touchpad::default(),
        }
    }
}

fn settings(schema: &str) -> Option<gio::Settings> {
    helix_settings::has_schema(schema).then(|| gio::Settings::new(schema))
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
    if let Some(s) = settings(MOUSE) {
        out.mouse = Pointer {
            speed: s.double("speed"),
            accel_profile: s.string("accel-profile").to_string(),
            natural_scroll: s.boolean("natural-scroll"),
            left_handed: s.boolean("left-handed"),
            middle_click_emulation: s.boolean("middle-click-emulation"),
        };
    }
    if let Some(s) = settings(TOUCHPAD) {
        let left_handed = match s.string("left-handed").as_str() {
            "left" => true,
            "right" => false,
            _ => out.mouse.left_handed,
        };
        out.touchpad = Touchpad {
            pointer: Pointer {
                speed: s.double("speed"),
                accel_profile: s.string("accel-profile").to_string(),
                natural_scroll: s.boolean("natural-scroll"),
                left_handed,
                middle_click_emulation: s.boolean("middle-click-emulation"),
            },
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
    fn repeat_values() {
        assert_eq!(repeat(true, 500, 30), (33, 500));
        assert_eq!(repeat(false, 500, 30), (0, 500));
        assert_eq!(repeat(true, 250, 0), (1000, 250));
    }
}
