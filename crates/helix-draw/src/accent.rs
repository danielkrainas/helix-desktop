//! The accent color (helix-desktop-spec `components/README.md`, "Accent
//! color"): the nine named accents of `org.gnome.desktop.interface
//! accent-color` and the shades Helix derives from each for its own
//! surfaces.
//!
//! This is the one table of accent colors in Helix: the portal reports
//! the named values to apps, the shell and the greeter draw with the
//! derived [`Palette`].

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

use crate::backdrop::contrast;
use crate::theme;

/// A named accent color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Accent {
    #[default]
    Blue,
    Teal,
    Green,
    Yellow,
    Orange,
    Red,
    Pink,
    Purple,
    Slate,
}

impl Accent {
    /// Every accent, in the schema's order.
    pub const ALL: [Accent; 9] = [Accent::Blue, Accent::Teal, Accent::Green, Accent::Yellow, Accent::Orange, Accent::Red, Accent::Pink, Accent::Purple, Accent::Slate];

    /// The name used by `accent-color` (and the greeter's `accent`).
    pub fn name(self) -> &'static str {
        match self {
            Accent::Blue => "blue",
            Accent::Teal => "teal",
            Accent::Green => "green",
            Accent::Yellow => "yellow",
            Accent::Orange => "orange",
            Accent::Red => "red",
            Accent::Pink => "pink",
            Accent::Purple => "purple",
            Accent::Slate => "slate",
        }
    }

    /// The accent called `name`; `None` for a name the schema lacks.
    pub fn from_name(name: &str) -> Option<Accent> {
        Accent::ALL.into_iter().find(|a| a.name() == name)
    }

    /// The accent as named, `0xRRGGBB`: the sRGB values libadwaita
    /// documents for its accent colors, which apps also receive.
    pub fn rgb(self) -> u32 {
        match self {
            Accent::Blue => 0x3584E4,
            Accent::Teal => 0x2190A4,
            Accent::Green => 0x3A944A,
            Accent::Yellow => 0xC88800,
            Accent::Orange => 0xED5B00,
            Accent::Red => 0xE62D42,
            Accent::Pink => 0xD56199,
            Accent::Purple => 0x9141AC,
            Accent::Slate => 0x6F8396,
        }
    }

    /// [`Accent::rgb`] as red, green and blue from 0 to 1.
    pub fn rgb_f64(self) -> (f64, f64, f64) {
        let channel = |shift: u32| f64::from((self.rgb() >> shift) & 0xFF) / 255.0;
        (channel(16), channel(8), channel(0))
    }

    /// The colors Helix draws this accent with.
    pub fn palette(self) -> &'static Palette {
        static TABLE: OnceLock<[Palette; 9]> = OnceLock::new();
        let table = TABLE.get_or_init(|| Accent::ALL.map(Palette::derive));
        &table[self as usize]
    }
}

/// What an accent is drawn with, every color `0xAARRGGBB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// The accent without text on it: focus rings, indicators, a slider's
    /// filled part, check marks. At least 3:1 on popovers.
    pub color: u32,
    /// The accent as a fill under text (switches on, the selected row,
    /// default buttons), at rest, hovered and pressed.
    pub fill: u32,
    pub fill_hover: u32,
    pub fill_pressed: u32,
    /// Text and symbols on the fills: light, or dark where light text
    /// would need the accent darkened too far (yellow).
    pub on_fill: u32,
    /// Accent-colored words (links, "Connected") on the dark surfaces.
    pub text: u32,
    /// A translucent highlight: selected desktop icons.
    pub selection: u32,
}

/// Text on an accent fill reaches this (some room above 4.5:1, so that
/// its hovered shade still keeps 4.5).
const FILL_TARGET: f32 = 5.0;
/// Text: WCAG AA.
const TEXT_MIN: f32 = 4.5;
/// Non-text UI (rings, indicators) against what is around it.
const UI_MIN: f32 = 3.0;
/// Light text is kept while the accent needs darkening by at most this
/// much (in 64ths) to carry it; past that, text on it is dark.
const MAX_DARKEN: u32 = 16;
/// Accent-colored words are the accent lightened at least this much (in
/// 64ths), so they read as text rather than as a ring.
const TEXT_LIGHTEN: u32 = 16;

const WHITE: u32 = 0xFFFF_FFFF;
const BLACK: u32 = 0xFF00_0000;

/// Dark text on light accents.
pub const DARK_ON_FILL: u32 = 0xFF1E1E24;

/// `a` mixed toward `b` by `k` 64ths (opaque).
fn mix(a: u32, b: u32, k: u32) -> u32 {
    let ch = |shift: u32| {
        let (x, y) = ((a >> shift) & 0xFF, (b >> shift) & 0xFF);
        ((x * (64 - k) + y * k + 32) / 64) << shift
    };
    0xFF00_0000 | ch(16) | ch(8) | ch(0)
}

/// The first of `color` mixed toward `target` by `from`, `from + 1`, …
/// 64ths that `ok` accepts (with the number of 64ths); `target` when none.
fn toward(color: u32, target: u32, from: u32, ok: impl Fn(u32) -> bool) -> (u32, u32) {
    (from..=64).map(|k| (mix(color, target, k), k)).find(|&(c, _)| ok(c)).unwrap_or((target, 64))
}

/// `fill` shaded toward `target` by up to `most` 64ths, stopping before
/// `fg` on it drops under 4.5:1.
fn shade(fill: u32, target: u32, most: u32, fg: u32) -> u32 {
    (1..=most).map(|k| mix(fill, target, k)).take_while(|&c| contrast(fg, c) >= TEXT_MIN).last().unwrap_or(fill)
}

impl Palette {
    /// The shades of `accent`, each checked against the surfaces of
    /// [`crate::theme`] it is drawn on.
    pub fn derive(accent: Accent) -> Palette {
        let base = 0xFF00_0000 | accent.rgb();
        let popover = theme::POPOVER;
        // Rings and indicators: lightened until they stand out from
        // popovers (and a slider's fill from its track).
        let (color, _) = toward(base, WHITE, 0, |c| contrast(c, popover) >= UI_MIN && contrast(c, theme::TRACK) >= 1.5);
        let (text, _) = toward(base, WHITE, TEXT_LIGHTEN, |c| contrast(c, popover) >= TEXT_MIN);
        // The fill: darkened under light text when that is a modest
        // change, else kept bright (or brightened) under dark text.
        let (_, darken) = toward(base, BLACK, 0, |c| contrast(theme::TEXT, c) >= TEXT_MIN);
        let (on_fill, away, hover_to, press_to) = if darken <= MAX_DARKEN { (theme::TEXT, BLACK, WHITE, BLACK) } else { (DARK_ON_FILL, WHITE, WHITE, BLACK) };
        let (fill, _) = toward(base, away, 0, |c| contrast(on_fill, c) >= FILL_TARGET);
        Palette {
            color,
            fill,
            fill_hover: shade(fill, hover_to, 6, on_fill),
            fill_pressed: shade(fill, press_to, 10, on_fill),
            on_fill,
            text,
            selection: (color & 0x00FF_FFFF) | 0x5500_0000,
        }
    }
}

/// The accent chosen, as its index in [`Accent::ALL`].
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// The process's accent: the shell follows `accent-color`, the greeter
/// its configuration; the compositor keeps blue.
pub fn set(accent: Accent) {
    CURRENT.store(accent as u8, Ordering::Relaxed);
}

/// The accent chosen with [`set`].
pub fn chosen() -> Accent {
    Accent::ALL[usize::from(CURRENT.load(Ordering::Relaxed)) % Accent::ALL.len()]
}

/// The colors to draw with now. High contrast keeps its own fixed colors
/// (blue's), whatever the accent.
pub fn current() -> &'static Palette {
    palette_for(chosen(), crate::high_contrast())
}

fn palette_for(accent: Accent, high_contrast: bool) -> &'static Palette {
    if high_contrast { Accent::Blue.palette() } else { accent.palette() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for a in Accent::ALL {
            assert_eq!(Accent::from_name(a.name()), Some(a));
        }
        assert_eq!(Accent::from_name("magenta"), None);
        assert_eq!(Accent::default(), Accent::Blue);
        assert_eq!(Accent::Slate.rgb_f64(), (f64::from(0x6F) / 255.0, f64::from(0x83) / 255.0, f64::from(0x96) / 255.0));
    }

    /// Every accent's derived colors meet their contrast on every surface
    /// they are drawn on.
    #[test]
    fn contrast_of_every_accent() {
        let opaque = |c: u32| c | 0xFF00_0000;
        let surfaces = [opaque(theme::PANEL), theme::POPOVER, theme::WELL];
        for a in Accent::ALL {
            let p = a.palette();
            let name = a.name();
            for bg in surfaces {
                let c = contrast(p.color, bg);
                assert!(c >= UI_MIN, "{name}: ring {:08x} on {bg:08x} is {c:.2}:1", p.color);
                let c = contrast(p.text, bg);
                assert!(c >= TEXT_MIN, "{name}: text {:08x} on {bg:08x} is {c:.2}:1", p.text);
            }
            let c = contrast(p.color, theme::TRACK);
            assert!(c >= 1.5, "{name}: slider fill on its track is {c:.2}:1");
            for fill in [p.fill, p.fill_hover, p.fill_pressed] {
                let c = contrast(p.on_fill, fill);
                assert!(c >= TEXT_MIN, "{name}: {:08x} on {fill:08x} is {c:.2}:1", p.on_fill);
            }
            // The shades differ, so hovering and pressing show.
            assert_ne!(p.fill_hover, p.fill, "{name}: hover");
            assert_ne!(p.fill_pressed, p.fill, "{name}: pressed");
            assert_eq!(p.selection >> 24, 0x55);
        }
    }

    #[test]
    fn label_colors() {
        assert_eq!(Accent::Yellow.palette().on_fill, DARK_ON_FILL, "dark text on yellow");
        for a in [Accent::Blue, Accent::Teal, Accent::Green, Accent::Orange, Accent::Red, Accent::Pink, Accent::Purple, Accent::Slate] {
            assert_eq!(a.palette().on_fill, theme::TEXT, "{}: light text", a.name());
        }
        // Blue's ring is the named blue itself.
        assert_eq!(Accent::Blue.palette().color, 0xFF3584E4);
    }

    #[test]
    fn high_contrast_ignores_the_accent() {
        assert_eq!(palette_for(Accent::Yellow, true), Accent::Blue.palette());
        assert_eq!(palette_for(Accent::Slate, true), Accent::Blue.palette());
        assert_eq!(palette_for(Accent::Yellow, false), Accent::Yellow.palette());
    }
}
