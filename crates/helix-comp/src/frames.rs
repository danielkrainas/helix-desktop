//! Title bars Helix draws for X11 windows (helix-desktop-spec
//! `contracts/wayland/README.md`, "Window frames"): their settings, layout
//! and hit testing. The compositor draws them and handles the clicks.

use gio::prelude::*;

pub const WM_PREFERENCES: &str = "org.gnome.desktop.wm.preferences";
pub const INTERFACE: &str = "org.gnome.desktop.interface";
pub const MOUSE: &str = "org.gnome.desktop.peripherals.mouse";
pub const MUTTER: &str = "org.gnome.mutter";
pub const SCHEMAS: [&str; 4] = [WM_PREFERENCES, INTERFACE, MOUSE, MUTTER];

/// Height of a title bar, in logical pixels.
pub const TITLE_H: i32 = 34;
/// Width a button takes in the title bar.
pub const BUTTON_SLOT: i32 = 30;
/// Diameter of a button's circle.
pub const BUTTON: i32 = 24;
/// Width of a `spacer`.
pub const SPACER: i32 = 12;
/// Space between the title bar's ends and the outermost buttons.
pub const PAD: i32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Minimize,
    Maximize,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Button(Button),
    Spacer,
}

/// `action-*-click-titlebar`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleAction {
    ToggleMaximize,
    ToggleMaximizeHorizontally,
    ToggleMaximizeVertically,
    Minimize,
    Lower,
    /// The window menu.
    Menu,
    None,
}

impl TitleAction {
    pub fn parse(value: &str) -> Self {
        match value {
            "toggle-maximize" => Self::ToggleMaximize,
            "toggle-maximize-horizontally" => Self::ToggleMaximizeHorizontally,
            "toggle-maximize-vertically" => Self::ToggleMaximizeVertically,
            "minimize" => Self::Minimize,
            "lower" => Self::Lower,
            "menu" => Self::Menu,
            // `toggle-shade` (no shading), `none`.
            _ => Self::None,
        }
    }
}

/// `button-layout`: the left and right corners' slots.
pub fn parse_layout(layout: &str) -> (Vec<Slot>, Vec<Slot>) {
    let mut seen = Vec::new();
    let mut side = |names: &str| -> Vec<Slot> {
        names
            .split(',')
            .filter_map(|name| {
                let slot = match name.trim() {
                    "minimize" => Slot::Button(Button::Minimize),
                    "maximize" => Slot::Button(Button::Maximize),
                    "close" => Slot::Button(Button::Close),
                    "spacer" => return Some(Slot::Spacer),
                    _ => return None,
                };
                if seen.contains(&slot) {
                    return None;
                }
                seen.push(slot);
                Some(slot)
            })
            .collect()
    };
    match layout.split_once(':') {
        Some((left, right)) => {
            let left = side(left);
            (left, side(right))
        }
        None => (side(layout), Vec::new()),
    }
}

/// Style words that may end a font family in a font description.
const STYLE_WORDS: [&str; 20] = [
    "Bold", "Italic", "Oblique", "Light", "Medium", "Regular", "Semi-Bold", "Semibold", "SemiBold", "Demi-Bold",
    "Heavy", "Black", "Thin", "Ultra-Light", "Book", "Condensed", "Extra-Bold", "Ultra-Bold", "Semi-Light", "Normal",
];

/// A font description (`Family [Styles] Size`, e.g. `Adwaita Sans Bold 11`):
/// the family and the size in pixels (at 96 dpi).
pub fn parse_font(description: &str) -> (String, f32) {
    let mut words: Vec<&str> = description.split_whitespace().collect();
    let mut points = 11.0;
    if let Some(size) = words.last().and_then(|w| w.parse::<f32>().ok()) {
        points = size;
        words.pop();
    }
    while words.len() > 1 && words.last().is_some_and(|w| STYLE_WORDS.iter().any(|s| s.eq_ignore_ascii_case(w))) {
        words.pop();
    }
    let family = if words.is_empty() { "sans-serif".to_owned() } else { words.join(" ") };
    (family, points * 96.0 / 72.0)
}

/// Everything about frames that comes from settings.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameConfig {
    pub left: Vec<Slot>,
    pub right: Vec<Slot>,
    /// Font family and size in pixels; titles are drawn bold.
    pub family: String,
    pub px: f32,
    pub dark: bool,
    pub double_click: TitleAction,
    pub middle_click: TitleAction,
    pub right_click: TitleAction,
    pub double_click_ms: u32,
    /// Width of the invisible resize border.
    pub border: i32,
}

impl Default for FrameConfig {
    fn default() -> Self {
        let (left, right) = parse_layout("appmenu:close");
        let (family, px) = parse_font("Adwaita Sans 11");
        Self {
            left,
            right,
            family,
            px,
            dark: false,
            double_click: TitleAction::ToggleMaximize,
            middle_click: TitleAction::None,
            right_click: TitleAction::Menu,
            double_click_ms: 400,
            border: 10,
        }
    }
}

fn settings_with(schema: &str, key: &str) -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    source.lookup(schema, true)?.has_key(key).then(|| gio::Settings::new(schema))
}

fn string(schema: &str, key: &str) -> Option<String> {
    settings_with(schema, key).map(|s| s.string(key).to_string())
}

/// The current settings (defaults where a schema or key is missing).
pub fn read() -> FrameConfig {
    let default = FrameConfig::default();
    let (left, right) = string(WM_PREFERENCES, "button-layout").map_or((default.left, default.right), |l| parse_layout(&l));
    let system = settings_with(WM_PREFERENCES, "titlebar-uses-system-font").is_none_or(|s| s.boolean("titlebar-uses-system-font"));
    let font = if system { string(INTERFACE, "font-name") } else { string(WM_PREFERENCES, "titlebar-font") };
    let (family, px) = font.map_or((default.family, default.px), |f| parse_font(&f));
    let action = |key: &str, fallback| string(WM_PREFERENCES, key).map_or(fallback, |v| TitleAction::parse(&v));
    FrameConfig {
        left,
        right,
        family,
        px,
        dark: string(INTERFACE, "color-scheme").as_deref() == Some("prefer-dark"),
        double_click: action("action-double-click-titlebar", default.double_click),
        middle_click: action("action-middle-click-titlebar", default.middle_click),
        right_click: action("action-right-click-titlebar", default.right_click),
        double_click_ms: settings_with(MOUSE, "double-click").map_or(default.double_click_ms, |s| s.int("double-click").max(1) as u32),
        border: settings_with(MUTTER, "draggable-border-width").map_or(default.border, |s| s.int("draggable-border-width").clamp(0, 64)),
    }
}

/// A rectangle in logical pixels: x, y, width, height.
pub type Rect = (i32, i32, i32, i32);

fn contains((x, y, w, h): Rect, (px, py): (f64, f64)) -> bool {
    px >= f64::from(x) && py >= f64::from(y) && px < f64::from(x + w) && py < f64::from(y + h)
}

/// Where the buttons of a `width`-wide title bar go (relative to it), and
/// the span left for the title (start, end).
pub fn layout(config: &FrameConfig, width: i32) -> (Vec<(Button, Rect)>, (i32, i32)) {
    let mut buttons = Vec::new();
    let mut x = PAD;
    for slot in &config.left {
        match slot {
            Slot::Button(b) => {
                buttons.push((*b, (x, 0, BUTTON_SLOT, TITLE_H)));
                x += BUTTON_SLOT;
            }
            Slot::Spacer => x += SPACER,
        }
    }
    let start = x;
    let mut x = width - PAD;
    for slot in config.right.iter().rev() {
        match slot {
            Slot::Button(b) => {
                x -= BUTTON_SLOT;
                buttons.push((*b, (x, 0, BUTTON_SLOT, TITLE_H)));
            }
            Slot::Spacer => x -= SPACER,
        }
    }
    (buttons, (start, x.max(start)))
}

/// What part of a frame a point is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Title,
    Button(Button),
    /// A resize border: xdg_toplevel edge bits (top 1, bottom 2, left 4,
    /// right 8).
    Edge(u32),
}

/// The part of the frame of a window whose own area is `client` that
/// `pos` is on; `None` inside the window or outside the frame.
pub fn hit(config: &FrameConfig, client: Rect, pos: (f64, f64)) -> Option<Part> {
    let (x, y, w, h) = client;
    if contains(client, pos) {
        return None;
    }
    let title = (x, y - TITLE_H, w, TITLE_H);
    if contains(title, pos) {
        let local = (pos.0 - f64::from(x), pos.1 - f64::from(y - TITLE_H));
        let (buttons, _) = layout(config, w);
        return Some(buttons.iter().find(|(_, r)| contains(*r, local)).map_or(Part::Title, |(b, _)| Part::Button(*b)));
    }
    let b = config.border;
    let top = y - TITLE_H;
    if b <= 0 || !contains((x - b, top - b, w + 2 * b, h + TITLE_H + 2 * b), pos) {
        return None;
    }
    let (px, py) = pos;
    let (left, right) = (px < f64::from(x), px >= f64::from(x + w));
    let (above, below) = (py < f64::from(top), py >= f64::from(y + h));
    let mut edges = (above as u32) | ((below as u32) << 1) | ((left as u32) << 2) | ((right as u32) << 3);
    // Corners reach along each edge by the border width.
    if left || right {
        if py < f64::from(top + b) {
            edges |= 1;
        } else if py >= f64::from(y + h - b) {
            edges |= 2;
        }
    }
    if above || below {
        if px < f64::from(x + b) {
            edges |= 4;
        } else if px >= f64::from(x + w - b) {
            edges |= 8;
        }
    }
    Some(Part::Edge(edges))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts() {
        assert_eq!(parse_layout("appmenu:close"), (vec![], vec![Slot::Button(Button::Close)]));
        assert_eq!(
            parse_layout("close,maximize,spacer:minimize,close"),
            (
                vec![Slot::Button(Button::Close), Slot::Button(Button::Maximize), Slot::Spacer],
                vec![Slot::Button(Button::Minimize)]
            ),
            "a repeated name is shown once"
        );
        assert_eq!(parse_layout("minimize,icon"), (vec![Slot::Button(Button::Minimize)], vec![]), "no colon: left");
    }

    #[test]
    fn fonts() {
        let (family, px) = parse_font("Adwaita Sans Bold 11");
        assert_eq!(family, "Adwaita Sans");
        assert!((px - 14.666).abs() < 0.01);
        assert_eq!(parse_font("Cantarell 12").0, "Cantarell");
        assert_eq!(parse_font("DejaVu Sans Semi-Bold Italic 10").0, "DejaVu Sans");
        assert_eq!(parse_font("").0, "sans-serif");
    }

    #[test]
    fn actions() {
        assert_eq!(TitleAction::parse("lower"), TitleAction::Lower);
        assert_eq!(TitleAction::parse("menu"), TitleAction::Menu);
        assert_eq!(TitleAction::parse("toggle-shade"), TitleAction::None);
    }

    #[test]
    fn hits() {
        let mut config = FrameConfig::default();
        (config.left, config.right) = parse_layout("minimize:maximize,close");
        let client = (100, 100, 400, 300);
        assert_eq!(hit(&config, client, (200.0, 200.0)), None, "inside the window");
        assert_eq!(hit(&config, client, (300.0, 80.0)), Some(Part::Title));
        assert_eq!(hit(&config, client, (110.0, 80.0)), Some(Part::Button(Button::Minimize)));
        assert_eq!(hit(&config, client, (490.0, 80.0)), Some(Part::Button(Button::Close)));
        assert_eq!(hit(&config, client, (465.0, 80.0)), Some(Part::Button(Button::Maximize)));
        // Borders, 10 wide by default, around window and title bar.
        assert_eq!(hit(&config, client, (95.0, 200.0)), Some(Part::Edge(4)), "left");
        assert_eq!(hit(&config, client, (300.0, 405.0)), Some(Part::Edge(2)), "bottom");
        assert_eq!(hit(&config, client, (300.0, 60.0)), Some(Part::Edge(1)), "above the title bar");
        assert_eq!(hit(&config, client, (95.0, 395.0)), Some(Part::Edge(6)), "bottom-left corner, along the left edge");
        assert_eq!(hit(&config, client, (505.0, 70.0)), Some(Part::Edge(9)), "top-right");
        assert_eq!(hit(&config, client, (600.0, 200.0)), None, "outside");
        let (_, title) = layout(&config, 400);
        assert_eq!(title, (PAD + BUTTON_SLOT, 400 - PAD - 2 * BUTTON_SLOT));
    }
}
