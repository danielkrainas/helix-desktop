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

/// Where on the monitor the magnified view is (`screen-position`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Position {
    #[default]
    FullScreen,
    TopHalf,
    BottomHalf,
    LeftHalf,
    RightHalf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub factor: f64,
    pub tracking: Tracking,
    /// `focus-tracking` and `caret-tracking`.
    pub focus_tracking: Tracking,
    pub caret_tracking: Tracking,
    pub position: Position,
    /// `lens-mode`: the view is a lens around the pointer.
    pub lens: bool,
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
            focus_tracking: Tracking::Proportional,
            caret_tracking: Tracking::Centered,
            position: Position::FullScreen,
            lens: false,
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

    /// Whether keyboard focus or the text caret is followed (needing the
    /// accessibility bus).
    pub fn follows_focus(&self) -> bool {
        self.active() && (self.focus_tracking != Tracking::None || self.caret_tracking != Tracking::None)
    }

    /// Whether only part of the monitor is magnified.
    pub fn partial(&self) -> bool {
        self.lens || self.position != Position::FullScreen
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
    let schema = s.settings_schema();
    let has = |k: &str| schema.as_ref().is_some_and(|x| x.has_key(k));
    let tracking_of = |k: &str, default: Tracking| {
        if !has(k) {
            return default;
        }
        match s.string(k).as_str() {
            "none" => Tracking::None,
            "centered" => Tracking::Centered,
            "push" => Tracking::Push,
            "proportional" => Tracking::Proportional,
            _ => default,
        }
    };
    let position = if has("screen-position") {
        match s.string("screen-position").as_str() {
            "top-half" => Position::TopHalf,
            "bottom-half" => Position::BottomHalf,
            "left-half" => Position::LeftHalf,
            "right-half" => Position::RightHalf,
            _ => Position::FullScreen,
        }
    } else {
        Position::FullScreen
    };
    Settings {
        enabled,
        factor: s.double("mag-factor").clamp(1.0, 32.0),
        tracking: tracking_of("mouse-tracking", Tracking::Proportional),
        focus_tracking: tracking_of("focus-tracking", d.focus_tracking),
        caret_tracking: tracking_of("caret-tracking", d.caret_tracking),
        position,
        lens: has("lens-mode") && s.boolean("lens-mode"),
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

/// The part of a `size` monitor (monitor coordinates) the magnified view
/// is drawn in: all of it, a half (`screen-position`), or with a lens a
/// half-size rectangle centered on the pointer, kept on the monitor.
pub fn viewport(settings: &Settings, size: (f64, f64), pointer: Option<(f64, f64)>) -> (f64, f64, f64, f64) {
    let (w, h) = size;
    if settings.lens {
        let (lw, lh) = (w / 2.0, h / 2.0);
        let p = pointer.unwrap_or((w / 2.0, h / 2.0));
        return ((p.0 - lw / 2.0).clamp(0.0, w - lw), (p.1 - lh / 2.0).clamp(0.0, h - lh), lw, lh);
    }
    match settings.position {
        Position::FullScreen => (0.0, 0.0, w, h),
        Position::TopHalf => (0.0, 0.0, w, h / 2.0),
        Position::BottomHalf => (0.0, h / 2.0, w, h / 2.0),
        Position::LeftHalf => (0.0, 0.0, w / 2.0, h),
        Position::RightHalf => (w / 2.0, 0.0, w / 2.0, h),
    }
}

/// The top-left corner of the shown part of a `size` monitor (logical,
/// monitor coordinates) for a view of `shown` size, given where it was
/// (`previous`) and the point followed (`target`, monitor coordinates;
/// `None` when not on this monitor) the way `tracking` says.
pub fn view_of(settings: &Settings, tracking: Tracking, size: (f64, f64), shown: (f64, f64), previous: Option<(f64, f64)>, target: Option<(f64, f64)>) -> (f64, f64) {
    let proportional = |p: (f64, f64)| (p.0 * (size.0 - shown.0) / size.0.max(1.0), p.1 * (size.1 - shown.1) / size.1.max(1.0));
    let clamp = |v: (f64, f64)| (v.0.clamp(0.0, (size.0 - shown.0).max(0.0)), v.1.clamp(0.0, (size.1 - shown.1).max(0.0)));
    let Some(p) = target else {
        return previous.unwrap_or_else(|| proportional((size.0 / 2.0, size.1 / 2.0)));
    };
    let Some(prev) = previous else { return proportional(p) };
    match tracking {
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

/// [`view_of`] for the whole monitor, following the pointer.
pub fn view(settings: &Settings, size: (f64, f64), previous: Option<(f64, f64)>, pointer: Option<(f64, f64)>) -> (f64, f64) {
    let m = settings.factor.max(1.0);
    view_of(settings, settings.tracking, size, (size.0 / m, size.1 / m), previous, pointer)
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
    fn partial_views() {
        let mut st = s(Tracking::Proportional);
        st.position = Position::BottomHalf;
        assert_eq!(viewport(&st, SIZE, None), (0.0, 400.0, 1000.0, 400.0));
        st.position = Position::RightHalf;
        assert_eq!(viewport(&st, SIZE, None), (500.0, 0.0, 500.0, 800.0));
        st.lens = true;
        assert_eq!(viewport(&st, SIZE, Some((100.0, 700.0))), (0.0, 400.0, 500.0, 400.0), "kept on the monitor");
        assert_eq!(viewport(&st, SIZE, Some((500.0, 400.0))), (250.0, 200.0, 500.0, 400.0));
        // A half-width view at 2x shows a quarter of the width.
        assert_eq!(view_of(&st, Tracking::Centered, SIZE, (250.0, 400.0), Some((0.0, 0.0)), Some((500.0, 400.0))), (375.0, 200.0));
        assert_eq!(view_of(&st, Tracking::Proportional, SIZE, (250.0, 400.0), None, Some((1000.0, 800.0))), (750.0, 400.0));
        assert!(st.partial());
        let mut f = s(Tracking::Proportional);
        assert!(f.follows_focus(), "caret tracking is on by default");
        f.focus_tracking = Tracking::None;
        f.caret_tracking = Tracking::None;
        assert!(!f.follows_focus());
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("#ff0000", 0.5), Some([1.0, 0.0, 0.0, 0.5]));
        assert_eq!(parse_color("#00ff0080", 1.0).map(|c| (c[1], (c[3] * 255.0).round())), Some((1.0, 128.0)));
        assert_eq!(parse_color("red", 1.0), None);
    }
}
