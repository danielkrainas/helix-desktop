//! The magnifier (helix-desktop-spec `contracts/gsettings/a11y-magnifier.md`):
//! settings, and which part of a monitor shows.

pub const MAGNIFIER: &str = "org.gnome.desktop.a11y.magnifier";
pub const APPLICATIONS: &str = "org.gnome.desktop.a11y.applications";
pub const SCHEMAS: [&str; 2] = [MAGNIFIER, APPLICATIONS];

/// `mouse-tracking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tracking {
    None,
    Centered,
    #[default]
    Proportional,
    Push,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub factor: f64,
    pub tracking: Tracking,
    pub scroll_at_edges: bool,
    pub cross_hairs: bool,
    /// RGBA, 0..1, opacity applied.
    pub cross_color: [f32; 4],
    pub cross_thickness: i32,
    pub cross_length: i32,
    pub cross_clip: bool,
    pub invert: bool,
    pub saturation: f32,
    pub contrast: [f32; 3],
    pub brightness: [f32; 3],
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            factor: 2.0,
            tracking: Tracking::Proportional,
            scroll_at_edges: false,
            cross_hairs: false,
            cross_color: [1.0, 0.0, 0.0, 0.66],
            cross_thickness: 8,
            cross_length: 4096,
            cross_clip: false,
            invert: false,
            saturation: 1.0,
            contrast: [0.0; 3],
            brightness: [0.0; 3],
        }
    }
}

impl Settings {
    /// Whether the screen is magnified.
    pub fn active(&self) -> bool {
        self.enabled && self.factor > 1.0
    }

    /// Whether colors are changed (while the magnifier is on).
    pub fn effects(&self) -> bool {
        self.enabled && (self.invert || self.saturation != 1.0 || self.contrast != [0.0; 3] || self.brightness != [0.0; 3])
    }
}

/// `#rrggbb` (or `#rrggbbaa`) with `opacity`.
pub fn parse_color(text: &str, opacity: f64) -> Option<[f32; 4]> {
    let hex = text.strip_prefix('#')?;
    if !(hex.len() == 6 || hex.len() == 8) || !hex.is_ascii() {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok().map(|b| f32::from(b) / 255.0);
    let alpha = if hex.len() == 8 { byte(6)? } else { 1.0 };
    Some([byte(0)?, byte(2)?, byte(4)?, alpha * opacity.clamp(0.0, 1.0) as f32])
}

fn settings_for(schema: &str) -> Option<gio::Settings> {
    gio::SettingsSchemaSource::default()?.lookup(schema, true).map(|_| gio::Settings::new(schema))
}

/// The current settings.
pub fn read() -> Settings {
    use gio::prelude::*;
    let d = Settings::default();
    let enabled = settings_for(APPLICATIONS).is_some_and(|s| s.boolean("screen-magnifier-enabled"));
    let Some(s) = settings_for(MAGNIFIER) else { return Settings { enabled, ..d } };
    let tracking = match s.string("mouse-tracking").as_str() {
        "none" => Tracking::None,
        "centered" => Tracking::Centered,
        "push" => Tracking::Push,
        _ => Tracking::Proportional,
    };
    Settings {
        enabled,
        factor: s.double("mag-factor").clamp(1.0, 32.0),
        tracking,
        scroll_at_edges: s.boolean("scroll-at-edges"),
        cross_hairs: s.boolean("show-cross-hairs"),
        cross_color: parse_color(&s.string("cross-hairs-color"), s.double("cross-hairs-opacity")).unwrap_or(d.cross_color),
        cross_thickness: s.int("cross-hairs-thickness").clamp(1, 100),
        cross_length: s.int("cross-hairs-length").clamp(20, 4096),
        cross_clip: s.boolean("cross-hairs-clip"),
        invert: s.boolean("invert-lightness"),
        saturation: s.double("color-saturation").clamp(0.0, 1.0) as f32,
        contrast: ["contrast-red", "contrast-green", "contrast-blue"].map(|k| s.double(k).clamp(-1.0, 1.0) as f32),
        brightness: ["brightness-red", "brightness-green", "brightness-blue"].map(|k| s.double(k).clamp(-1.0, 1.0) as f32),
    }
}

/// The top-left corner of the shown part of a `size` monitor (logical,
/// monitor coordinates), given where it was (`previous`) and the pointer
/// (`pointer`, monitor coordinates; `None` when not on this monitor).
pub fn view(settings: &Settings, size: (f64, f64), previous: Option<(f64, f64)>, pointer: Option<(f64, f64)>) -> (f64, f64) {
    let m = settings.factor.max(1.0);
    let shown = (size.0 / m, size.1 / m);
    let proportional = |p: (f64, f64)| (p.0 * (1.0 - 1.0 / m), p.1 * (1.0 - 1.0 / m));
    let clamp = |v: (f64, f64)| (v.0.clamp(0.0, size.0 - shown.0), v.1.clamp(0.0, size.1 - shown.1));
    let Some(p) = pointer else {
        return previous.unwrap_or_else(|| proportional((size.0 / 2.0, size.1 / 2.0)));
    };
    let Some(prev) = previous else { return proportional(p) };
    match settings.tracking {
        Tracking::Proportional => proportional(p),
        Tracking::None => clamp(prev),
        Tracking::Centered => {
            let v = (p.0 - shown.0 / 2.0, p.1 - shown.1 / 2.0);
            if settings.scroll_at_edges {
                v
            } else {
                clamp(v)
            }
        }
        Tracking::Push => {
            let axis = |v: f64, p: f64, len: f64| {
                if p < v {
                    p
                } else if p > v + len {
                    p - len
                } else {
                    v
                }
            };
            clamp((axis(prev.0, p.0, shown.0), axis(prev.1, p.1, shown.1)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(tracking: Tracking) -> Settings {
        Settings { enabled: true, factor: 2.0, tracking, ..Settings::default() }
    }

    const SIZE: (f64, f64) = (1000.0, 800.0);

    #[test]
    fn proportional_keeps_the_pointer_in_place() {
        let st = s(Tracking::Proportional);
        assert_eq!(view(&st, SIZE, Some((0.0, 0.0)), Some((0.0, 0.0))), (0.0, 0.0));
        assert_eq!(view(&st, SIZE, Some((0.0, 0.0)), Some((1000.0, 800.0))), (500.0, 400.0));
        // The pointer at 600: shown from 300, the pointer at (600-300)*2 = 600.
        assert_eq!(view(&st, SIZE, None, Some((600.0, 400.0))), (300.0, 200.0));
    }

    #[test]
    fn centered_and_edges() {
        let mut st = s(Tracking::Centered);
        assert_eq!(view(&st, SIZE, Some((0.0, 0.0)), Some((500.0, 400.0))), (250.0, 200.0));
        assert_eq!(view(&st, SIZE, Some((0.0, 0.0)), Some((10.0, 10.0))), (0.0, 0.0), "clamped");
        st.scroll_at_edges = true;
        assert_eq!(view(&st, SIZE, Some((0.0, 0.0)), Some((10.0, 10.0))), (-240.0, -190.0));
    }

    #[test]
    fn push_and_none() {
        let st = s(Tracking::Push);
        // Shown part 500x400 at (100,100): a pointer inside moves nothing.
        assert_eq!(view(&st, SIZE, Some((100.0, 100.0)), Some((300.0, 300.0))), (100.0, 100.0));
        assert_eq!(view(&st, SIZE, Some((100.0, 100.0)), Some((700.0, 50.0))), (200.0, 50.0));
        let none = s(Tracking::None);
        assert_eq!(view(&none, SIZE, Some((100.0, 100.0)), Some((900.0, 700.0))), (100.0, 100.0));
        // Off this monitor: stays.
        assert_eq!(view(&st, SIZE, Some((42.0, 7.0)), None), (42.0, 7.0));
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("#ff0000", 0.5), Some([1.0, 0.0, 0.0, 0.5]));
        assert_eq!(parse_color("#00ff0080", 1.0).map(|c| (c[1], (c[3] * 255.0).round())), Some((1.0, 128.0)));
        assert_eq!(parse_color("red", 1.0), None);
    }
}
