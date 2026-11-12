//! What X11 apps learn about the desktop (helix-desktop-spec
//! `contracts/wayland/README.md`, "X11 apps: scaling and settings"): the
//! Xwayland scale, and the XSETTINGS and X resources Helix publishes
//! (the freedesktop XSETTINGS specification).

/// Schemas the published settings come from.
pub const INTERFACE: &str = "org.gnome.desktop.interface";
pub const WM_PREFERENCES: &str = "org.gnome.desktop.wm.preferences";
pub const SOUND: &str = "org.gnome.desktop.sound";
pub const MOUSE: &str = "org.gnome.desktop.peripherals.mouse";
pub const MUTTER_WAYLAND: &str = "org.gnome.mutter.wayland";
pub const SCHEMAS: [&str; 5] = [INTERFACE, WM_PREFERENCES, SOUND, MOUSE, MUTTER_WAYLAND];

/// The largest Xwayland scale Helix uses.
pub const MAX_SCALE: i32 = 4;

/// The scale of Xwayland's coordinate space: `xwayland-scaling-factor`
/// rounded to a whole number, or (0: automatic) the largest monitor scale
/// rounded so; between 1 and [`MAX_SCALE`].
pub fn xwayland_scale(setting: f64, monitor_scales: &[f64]) -> i32 {
    let wanted = if setting > 0.0 { setting } else { monitor_scales.iter().copied().fold(1.0, f64::max) };
    (wanted.round() as i32).clamp(1, MAX_SCALE)
}

/// A setting's value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i32),
    Text(String),
}

/// The desktop settings X11 apps are told.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub gtk_theme: String,
    pub icon_theme: String,
    pub cursor_theme: String,
    pub cursor_size: i32,
    pub font_name: String,
    pub text_scaling: f64,
    /// `none`, `grayscale` or `rgba`.
    pub antialiasing: String,
    /// `none`, `slight`, `medium` or `full`.
    pub hinting: String,
    /// `rgb`, `bgr`, `vrgb`, `vbgr` (or `rgba`: unknown).
    pub rgba_order: String,
    pub cursor_blink: bool,
    pub cursor_blink_time: i32,
    pub cursor_blink_timeout: i32,
    pub animations: bool,
    pub overlay_scrolling: bool,
    pub key_theme: String,
    pub button_layout: String,
    pub event_sounds: bool,
    pub sound_theme: String,
    pub double_click: i32,
    /// `xwayland-scaling-factor`.
    pub xwayland_scaling: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            gtk_theme: "Adwaita".into(),
            icon_theme: "Adwaita".into(),
            cursor_theme: "Adwaita".into(),
            cursor_size: 24,
            font_name: "Adwaita Sans 11".into(),
            text_scaling: 1.0,
            antialiasing: "grayscale".into(),
            hinting: "slight".into(),
            rgba_order: "rgb".into(),
            cursor_blink: true,
            cursor_blink_time: 1200,
            cursor_blink_timeout: 10,
            animations: true,
            overlay_scrolling: true,
            key_theme: "Default".into(),
            button_layout: "appmenu:close".into(),
            event_sounds: true,
            sound_theme: "freedesktop".into(),
            double_click: 400,
            xwayland_scaling: 0.0,
        }
    }
}

/// The dots per inch fonts are drawn at (before the Xwayland scale).
fn dpi(s: &Settings) -> f64 {
    96.0 * s.text_scaling.clamp(0.5, 3.0)
}

/// The subpixel order, as Xft names it.
fn rgba(s: &Settings) -> &str {
    if s.antialiasing != "rgba" {
        return "none";
    }
    match s.rgba_order.as_str() {
        o @ ("rgb" | "bgr" | "vrgb" | "vbgr") => o,
        _ => "rgb",
    }
}

/// The XSETTINGS for `s` at Xwayland scale `scale`, sorted by name.
pub fn values(s: &Settings, scale: i32) -> Vec<(&'static str, Value)> {
    let int = |b: bool| Value::Int(i32::from(b));
    let text = |t: &str| Value::Text(t.to_owned());
    let mut v = vec![
        ("Gdk/UnscaledDPI", Value::Int((dpi(s) * 1024.0).round() as i32)),
        ("Gdk/WindowScalingFactor", Value::Int(scale)),
        ("Gtk/CursorBlinkTimeout", Value::Int(s.cursor_blink_timeout)),
        ("Gtk/CursorThemeName", text(&s.cursor_theme)),
        ("Gtk/CursorThemeSize", Value::Int(s.cursor_size * scale)),
        ("Gtk/DecorationLayout", text(&s.button_layout)),
        ("Gtk/EnableAnimations", int(s.animations)),
        ("Gtk/FontName", text(&s.font_name)),
        ("Gtk/KeyThemeName", text(&s.key_theme)),
        ("Gtk/OverlayScrolling", int(s.overlay_scrolling)),
        ("Net/CursorBlink", int(s.cursor_blink)),
        ("Net/CursorBlinkTime", Value::Int(s.cursor_blink_time)),
        ("Net/DoubleClickTime", Value::Int(s.double_click)),
        ("Net/EnableEventSounds", int(s.event_sounds)),
        ("Net/IconThemeName", text(&s.icon_theme)),
        ("Net/SoundThemeName", text(&s.sound_theme)),
        ("Net/ThemeName", text(&s.gtk_theme)),
        ("Xft/Antialias", int(s.antialiasing != "none")),
        ("Xft/DPI", Value::Int((dpi(s) * f64::from(scale) * 1024.0).round() as i32)),
        ("Xft/HintStyle", Value::Text(format!("hint{}", if s.hinting == "none" { "none" } else { &s.hinting }))),
        ("Xft/Hinting", int(s.hinting != "none")),
        ("Xft/RGBA", text(rgba(s))),
    ];
    v.sort_by_key(|(name, _)| *name);
    v
}

fn pad4(n: usize) -> usize {
    (4 - n % 4) % 4
}

/// The `_XSETTINGS_SETTINGS` property: little-endian, every setting last
/// changed at `serial`.
pub fn encode(values: &[(&str, Value)], serial: u32) -> Vec<u8> {
    let mut out = Vec::new();
    // Byte order (LSBFirst), padding, serial, number of settings.
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    for (name, value) in values {
        let kind: u8 = match value {
            Value::Int(_) => 0,
            Value::Text(_) => 1,
        };
        out.extend_from_slice(&[kind, 0]);
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend(std::iter::repeat_n(0, pad4(name.len())));
        out.extend_from_slice(&serial.to_le_bytes());
        match value {
            Value::Int(i) => out.extend_from_slice(&i.to_le_bytes()),
            Value::Text(t) => {
                out.extend_from_slice(&(t.len() as u32).to_le_bytes());
                out.extend_from_slice(t.as_bytes());
                out.extend(std::iter::repeat_n(0, pad4(t.len())));
            }
        }
    }
    out
}

/// The X resources Helix sets (as `Xft.dpi:\t192` lines).
pub fn resources(s: &Settings, scale: i32) -> Vec<(&'static str, String)> {
    let style = if s.hinting == "none" { "hintnone".to_owned() } else { format!("hint{}", s.hinting) };
    vec![
        ("Xft.dpi", ((dpi(s) * f64::from(scale)).round() as i32).to_string()),
        ("Xft.antialias", i32::from(s.antialiasing != "none").to_string()),
        ("Xft.hinting", i32::from(s.hinting != "none").to_string()),
        ("Xft.hintstyle", style),
        ("Xft.rgba", rgba(s).to_owned()),
        ("Xcursor.theme", s.cursor_theme.clone()),
        ("Xcursor.size", (s.cursor_size * scale).to_string()),
    ]
}

/// `RESOURCE_MANAGER`'s text with Helix's resources replacing earlier
/// values of the same names (other lines are kept).
pub fn merge_resources(existing: &str, ours: &[(&str, String)]) -> String {
    let mut out: String = existing
        .lines()
        .filter(|l| {
            let name = l.split(':').next().unwrap_or("").trim();
            !l.trim().is_empty() && !ours.iter().any(|(n, _)| *n == name)
        })
        .map(|l| format!("{l}\n"))
        .collect();
    for (name, value) in ours {
        out.push_str(&format!("{name}:\t{value}\n"));
    }
    out
}

/// Read the settings from GSettings (keys missing from the installed
/// schemas keep their defaults).
pub fn read() -> Settings {
    use gio::prelude::*;
    let source = gio::SettingsSchemaSource::default();
    let with = |schema: &str, key: &str| -> Option<gio::Settings> {
        source.as_ref()?.lookup(schema, true)?.has_key(key).then(|| gio::Settings::new(schema))
    };
    let string = |schema: &str, key: &str, d: &str| with(schema, key).map_or_else(|| d.to_owned(), |s| s.string(key).to_string());
    let int = |schema: &str, key: &str, d: i32| with(schema, key).map_or(d, |s| s.int(key));
    let boolean = |schema: &str, key: &str, d: bool| with(schema, key).map_or(d, |s| s.boolean(key));
    let d = Settings::default();
    Settings {
        gtk_theme: string(INTERFACE, "gtk-theme", &d.gtk_theme),
        icon_theme: string(INTERFACE, "icon-theme", &d.icon_theme),
        cursor_theme: string(INTERFACE, "cursor-theme", &d.cursor_theme),
        cursor_size: int(INTERFACE, "cursor-size", d.cursor_size),
        font_name: string(INTERFACE, "font-name", &d.font_name),
        text_scaling: with(INTERFACE, "text-scaling-factor").map_or(d.text_scaling, |s| s.double("text-scaling-factor")),
        antialiasing: string(INTERFACE, "font-antialiasing", &d.antialiasing),
        hinting: string(INTERFACE, "font-hinting", &d.hinting),
        rgba_order: string(INTERFACE, "font-rgba-order", &d.rgba_order),
        cursor_blink: boolean(INTERFACE, "cursor-blink", d.cursor_blink),
        cursor_blink_time: int(INTERFACE, "cursor-blink-time", d.cursor_blink_time),
        cursor_blink_timeout: int(INTERFACE, "cursor-blink-timeout", d.cursor_blink_timeout),
        animations: boolean(INTERFACE, "enable-animations", d.animations),
        overlay_scrolling: boolean(INTERFACE, "overlay-scrolling", d.overlay_scrolling),
        key_theme: string(INTERFACE, "gtk-key-theme", &d.key_theme),
        button_layout: string(WM_PREFERENCES, "button-layout", &d.button_layout),
        event_sounds: boolean(SOUND, "event-sounds", d.event_sounds),
        sound_theme: string(SOUND, "theme-name", &d.sound_theme),
        double_click: int(MOUSE, "double-click", d.double_click),
        xwayland_scaling: with(MUTTER_WAYLAND, "xwayland-scaling-factor").map_or(0.0, |s| s.double("xwayland-scaling-factor")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales() {
        assert_eq!(xwayland_scale(0.0, &[]), 1);
        assert_eq!(xwayland_scale(0.0, &[1.0, 1.25]), 1);
        assert_eq!(xwayland_scale(0.0, &[1.0, 1.5]), 2);
        assert_eq!(xwayland_scale(0.0, &[2.0, 3.0]), 3);
        assert_eq!(xwayland_scale(2.4, &[1.0]), 2);
        assert_eq!(xwayland_scale(0.4, &[2.0]), 1, "set: the setting rules");
        assert_eq!(xwayland_scale(9.0, &[]), MAX_SCALE);
    }

    #[test]
    fn values_and_encoding() {
        let s = Settings { antialiasing: "rgba".into(), rgba_order: "bgr".into(), text_scaling: 1.25, ..Settings::default() };
        let v = values(&s, 2);
        let get = |n: &str| v.iter().find(|(k, _)| *k == n).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(get("Gdk/WindowScalingFactor"), Value::Int(2));
        assert_eq!(get("Xft/DPI"), Value::Int(245_760), "96 × 1.25 × 2 × 1024");
        assert_eq!(get("Gdk/UnscaledDPI"), Value::Int(122_880));
        assert_eq!(get("Gtk/CursorThemeSize"), Value::Int(48));
        assert_eq!(get("Xft/RGBA"), Value::Text("bgr".into()));
        assert_eq!(get("Xft/HintStyle"), Value::Text("hintslight".into()));
        assert!(v.windows(2).all(|w| w[0].0 < w[1].0));

        let bytes = encode(&[("Net/ThemeName", Value::Text("Adw".into())), ("Xft/DPI", Value::Int(98_304))], 7);
        let mut want = vec![0u8, 0, 0, 0, 7, 0, 0, 0, 2, 0, 0, 0];
        want.extend([1, 0, 13, 0]);
        want.extend(b"Net/ThemeName\0\0\0");
        want.extend([7, 0, 0, 0, 3, 0, 0, 0]);
        want.extend(b"Adw\0");
        want.extend([0, 0, 7, 0]);
        want.extend(b"Xft/DPI\0");
        want.extend([7, 0, 0, 0]);
        want.extend(98_304i32.to_le_bytes());
        assert_eq!(bytes, want);
    }

    #[test]
    fn x_resources() {
        let r = resources(&Settings::default(), 2);
        assert_eq!(r[0], ("Xft.dpi", "192".to_owned()));
        assert_eq!(r.iter().find(|(n, _)| *n == "Xcursor.size").unwrap().1, "48");
        let merged = merge_resources("Xft.dpi:\t96\n*background:\tblack\n", &r[..1]);
        assert_eq!(merged, "*background:\tblack\nXft.dpi:\t192\n");
    }
}
