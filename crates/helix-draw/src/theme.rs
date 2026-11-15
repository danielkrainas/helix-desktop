//! The shell's look, shared by its surfaces: colors with names and the
//! pieces built from them (helix-desktop-spec `components/README.md`,
//! "Shell look").

use std::sync::atomic::{AtomicU32, Ordering};

use crate::Canvas;

/// The text scale (`org.gnome.desktop.interface text-scaling-factor`), as
/// f32 bits.
static TEXT_SCALE: AtomicU32 = AtomicU32::new(0x3F80_0000);

/// Every text size is multiplied by `factor` (Large Text: 1.25; kept
/// within 0.5 to 3). The process's own setting: the shell sets it, the
/// compositor and the greeter keep 1.
pub fn set_text_scale(factor: f64) {
    let factor = if factor.is_finite() { factor.clamp(0.5, 3.0) } else { 1.0 };
    TEXT_SCALE.store((factor as f32).to_bits(), Ordering::Relaxed);
}

pub fn text_scale() -> f32 {
    f32::from_bits(TEXT_SCALE.load(Ordering::Relaxed))
}

/// A length that holds text (a row's height, a menu's width), grown with
/// the text scale.
pub fn grow(v: i32) -> i32 {
    grown(v, text_scale())
}

fn grown(v: i32, scale: f32) -> i32 {
    (v as f32 * scale).round() as i32
}

// ----- Surfaces and controls -----------------------------------------------
//
// Steps between neighbours are at least 1.3:1 (WCAG contrast ratio), so a
// control, a hovered row and the popover under them can be told apart;
// `tests::steps` checks them.

/// The panel's background.
pub const PANEL: u32 = 0xF01E1E24;
/// Popovers (menus, quick settings, the notification list) and dialogs.
pub const POPOVER: u32 = 0xFF2E2E36;
/// A popover's or dialog's edge, separators.
pub const BORDER: u32 = 0xFF4A4A55;
/// A control at rest or off: switches, buttons, keys.
pub const CONTROL: u32 = 0xFF45454F;
/// A control or row under the pointer or the keyboard selection.
pub const CONTROL_HOVER: u32 = 0xFF5A5A64;
/// A control being pressed.
pub const CONTROL_PRESSED: u32 = 0xFF686872;
/// A slider's empty track.
pub const TRACK: u32 = 0xFF5E5E68;
/// A list set into a popover (networks, devices, a prompt's rows).
pub const WELL: u32 = 0xFF1E1E24;

// ----- The accent ------------------------------------------------------------
//
// Follows the accent chosen (`crate::accent`); fixed in high contrast.
// `crate::accent::tests` checks every accent's contrast.

/// The accent: focus rings, a slider's filled part, indicators.
pub fn accent() -> u32 {
    crate::accent::current().color
}

/// The accent as a fill under text (switches on, the selected row):
/// shaded so that [`on_accent`] text on it reaches 4.5:1.
pub fn accent_fill() -> u32 {
    crate::accent::current().fill
}

/// Text and symbols on [`accent_fill`] (and its hovered and pressed
/// shades): light, or dark on a light accent.
pub fn on_accent() -> u32 {
    crate::accent::current().on_fill
}

// ----- Text ------------------------------------------------------------------
//
// Each reaches 4.5:1 on the surfaces it is meant for; `tests::text`
// checks them.

/// Text on any surface or control.
pub const TEXT: u32 = 0xFFEDEDEF;
/// Secondary text, on the panel, popovers, dialogs and wells (not on
/// controls: use [`TEXT`] there).
pub const TEXT_DIM: u32 = 0xFFA4A4AD;
/// Errors and the "Sharing"/"Recording" indicators' words, on dark
/// surfaces (the saturated red is for fills and rings only).
pub const TEXT_ERROR: u32 = 0xFFFF7B63;
/// Accent-colored words (links, "Connected") on dark surfaces.
pub fn text_accent() -> u32 {
    crate::accent::current().text
}
/// Red fills: a recording dot, an error ring.
pub const ERROR: u32 = crate::screen::ERROR;

// ----- Pieces ----------------------------------------------------------------
//
// In high contrast (`crate::high_contrast`) every piece is black or the
// accent, and what tells them apart is a white outline: popovers,
// separators, controls that are off, wells, slider tracks, the hovered
// row.

/// High contrast's lines and text.
const WHITE: u32 = 0xFFFFFFFF;
const BLACK: u32 = 0xFF000000;

/// A `width` px outline just inside (x, y, w, h).
pub fn outline(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32, width: i32, color: u32) {
    canvas.rect(x, y, w, width, color);
    canvas.rect(x, y + h - width, w, width, color);
    canvas.rect(x, y + width, width, h - 2 * width, color);
    canvas.rect(x + w - width, y + width, width, h - 2 * width, color);
}

/// A popover or dialog box at (x, y, w, h): its fill and a 1 px edge
/// outside it (2 px white in high contrast).
pub fn popover(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32) {
    if crate::high_contrast() {
        canvas.rect(x, y, w, h, BLACK);
        outline(canvas, x - 2, y - 2, w + 4, h + 4, 2, WHITE);
        return;
    }
    canvas.rect(x - 1, y - 1, w + 2, h + 2, BORDER);
    canvas.rect(x, y, w, h, POPOVER);
}

/// A horizontal separator `w` wide at (x, y) (white in high contrast).
pub fn separator(canvas: &mut Canvas, x: i32, y: i32, w: i32) {
    canvas.rect(x, y, w, 1, if crate::high_contrast() { WHITE } else { BORDER });
}

/// A vertical separator `h` high at (x, y).
pub fn vseparator(canvas: &mut Canvas, x: i32, y: i32, h: i32) {
    canvas.rect(x, y, 1, h, if crate::high_contrast() { WHITE } else { BORDER });
}

/// How a control looks at a moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Look {
    /// A switch or choice that is on (accent).
    pub on: bool,
    pub hovered: bool,
    pub pressed: bool,
}

/// The fill of a control in `look`.
pub fn control_fill(look: Look) -> u32 {
    let accent = crate::accent::current();
    match (look.on, look.pressed, look.hovered) {
        (true, true, _) => accent.fill_pressed,
        (true, false, true) => accent.fill_hover,
        (true, false, false) => accent.fill,
        (false, true, _) => CONTROL_PRESSED,
        (false, false, true) => CONTROL_HOVER,
        (false, false, false) => CONTROL,
    }
}

/// The color of text (and symbolic icons) on a control in `look`.
pub fn control_text(look: Look) -> u32 {
    if look.on { on_accent() } else { TEXT }
}

/// A control (a switch, a choice, a button, a key) at (x, y, w, h). High
/// contrast: on is the accent, off black, both with a white outline (2 px
/// while hovered or pressed).
pub fn control(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32, look: Look) {
    control_in(canvas, (x, y, w, h), look, crate::high_contrast());
}

fn control_in(canvas: &mut Canvas, (x, y, w, h): (i32, i32, i32, i32), look: Look, hc: bool) {
    if hc {
        canvas.rect(x, y, w, h, if look.on { crate::accent::Accent::Blue.palette().fill } else { BLACK });
        outline(canvas, x, y, w, h, if look.hovered || look.pressed { 2 } else { 1 }, WHITE);
        return;
    }
    canvas.rect(x, y, w, h, control_fill(look));
}

/// The highlight of a hovered or keyboard-selected row (a 2 px white
/// outline in high contrast).
pub fn row_highlight(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32) {
    if crate::high_contrast() {
        outline(canvas, x, y, w, h, 2, WHITE);
        return;
    }
    canvas.rect(x, y, w, h, CONTROL_HOVER);
}

/// A list's well at (x, y, w, h) (outlined in high contrast).
pub fn well(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32) {
    canvas.rect(x, y, w, h, WELL);
    if crate::high_contrast() {
        outline(canvas, x, y, w, h, 1, WHITE);
    }
}

/// A slider at (x, y, w, h) (the band it lies in), filled to `fraction`
/// (0 to 1); `dim`: it cannot be changed or is muted. A rounded track
/// (6 px) with its filled part, and a round knob (18 px across) at the
/// value. High contrast: the track and knob are outlined in white.
pub fn slider(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32, fraction: f64, dim: bool) {
    let hc = crate::high_contrast();
    let knob = SLIDER_KNOB;
    // The knob's center travels between the track's ends.
    let (tx, tw) = (f64::from(x) + knob / 2.0, (f64::from(w) - knob).max(1.0));
    let track_h = 6.0;
    let ty = f64::from(y) + f64::from(h) / 2.0 - track_h / 2.0;
    canvas.round_rect(tx - track_h / 2.0, ty, tw + track_h, track_h, track_h / 2.0, if hc { BLACK } else { TRACK });
    if hc {
        canvas.round_outline(tx - track_h / 2.0, ty, tw + track_h, track_h, track_h / 2.0, 1.0, WHITE);
    }
    let at = tx + tw * fraction.clamp(0.0, 1.0);
    let fill = if dim { TEXT_DIM } else { accent() };
    canvas.round_rect(tx - track_h / 2.0, ty, at - tx + track_h, track_h, track_h / 2.0, fill);
    let cy = f64::from(y) + f64::from(h) / 2.0;
    canvas.circle(at, cy, knob / 2.0, if hc { BLACK } else if dim { CONTROL_HOVER } else { TEXT });
    if hc {
        canvas.ring(at, cy, knob / 2.0 - 1.0, 2.0, WHITE);
    }
}

/// A slider's knob, across (logical px).
pub const SLIDER_KNOB: f64 = 18.0;

/// A workspace dot at (x, y): the active one wide and filled; in high
/// contrast the others are hollow, so it is not told by width alone.
pub fn workspace_dot(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32, active: bool) {
    if crate::high_contrast() && !active {
        outline(canvas, x, y, w, h, 1, WHITE);
        return;
    }
    canvas.rect(x, y, w, h, if active { TEXT } else { TEXT_DIM });
}

/// The keyboard focus ring's color: the accent, white in high contrast.
pub fn focus() -> u32 {
    if crate::high_contrast() { WHITE } else { accent() }
}

/// How far outside its control the focus ring is, and how wide.
pub const FOCUS_GAP: f64 = 2.0;
pub const FOCUS_WIDTH: f64 = 2.0;

/// The focus ring of a square-cornered control at (x, y, w, h): 2 logical
/// px wide, 2 px outside it. Shown for keyboard focus only (never for the
/// pointer), on every control the keyboard can reach.
pub fn focus_ring(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32) {
    focus_ring_round(canvas, f64::from(x), f64::from(y), f64::from(w), f64::from(h), 0.0);
}

/// The focus ring of a control with corner radius `radius`: it follows the
/// corners (its own radius grows by the gap).
pub fn focus_ring_round(canvas: &mut Canvas, x: f64, y: f64, w: f64, h: f64, radius: f64) {
    let g = FOCUS_GAP;
    let r = if radius > 0.0 { radius + g } else { 0.0 };
    // (The outline is drawn outside the box it is given.)
    canvas.round_outline(x - g, y - g, w + 2.0 * g, h + 2.0 * g, r, FOCUS_WIDTH, focus());
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::backdrop::contrast;

    /// Neighbouring surfaces and controls are told apart (at least
    /// 1.3:1), the slider's parts and the focus ring stand out (3:1
    /// against what is around them where they carry meaning).
    #[test]
    fn steps() {
        let opaque = |c: u32| c | 0xFF00_0000;
        for (a, b, min, what) in [
            (CONTROL, POPOVER, 1.3, "a control on a popover"),
            (CONTROL_HOVER, CONTROL, 1.3, "a hovered control"),
            (CONTROL_HOVER, POPOVER, 1.3, "a hovered row"),
            (CONTROL_PRESSED, CONTROL_HOVER, 1.2, "a pressed control"),
            (CONTROL, opaque(PANEL), 1.3, "a key on the panel color"),
            (TRACK, POPOVER, 1.8, "a slider's track"),
            (accent(), TRACK, 1.5, "a slider's filled part"),
            (WELL, POPOVER, 1.2, "a list's well"),
            (BORDER, POPOVER, 1.3, "a popover's edge"),
            (accent(), POPOVER, 3.0, "the focus ring"),
        ] {
            let c = contrast(a, b);
            assert!(c >= min, "{what}: {a:08x} on {b:08x} is {c:.2}:1");
        }
    }

    /// Every text color on every surface it is drawn on reaches 4.5:1.
    #[test]
    fn text() {
        let opaque = |c: u32| c | 0xFF00_0000;
        let surfaces = [opaque(PANEL), POPOVER, WELL];
        for fg in [TEXT, TEXT_DIM, TEXT_ERROR, text_accent()] {
            for bg in surfaces {
                let c = contrast(fg, bg);
                assert!(c >= 4.5, "{fg:08x} on {bg:08x} is {c:.2}:1");
            }
        }
        for look in [Look::default(), Look { hovered: true, ..Default::default() }, Look { pressed: true, ..Default::default() }, Look { on: true, ..Default::default() }, Look { on: true, hovered: true, ..Default::default() }, Look { on: true, pressed: true, ..Default::default() }] {
            let bg = control_fill(look);
            let c = contrast(control_text(look), bg);
            assert!(c >= 4.5, "text on {bg:08x} is {c:.2}:1");
        }
    }

    #[test]
    fn high_contrast_outlines() {
        let mut data = vec![0u8; 40 * 40 * 4];
        let mut canvas = Canvas::scaled(&mut data, 40, 40, 1.0);
        control_in(&mut canvas, (5, 5, 30, 20), Look::default(), true);
        let px = |x: usize, y: usize| u32::from_le_bytes(data[(y * 40 + x) * 4..][..4].try_into().unwrap());
        assert_eq!(px(15, 5), 0xFFFFFFFF, "an off control is outlined in white");
        assert_eq!(px(15, 15), 0xFF000000, "and black inside");
    }

    #[test]
    fn growing() {
        assert_eq!(grown(32, 1.0), 32);
        assert_eq!(grown(32, 1.25), 40);
        assert_eq!(grown(32, 1.5), 48);
        assert_eq!(grown(15, 1.25), 19);
    }

    #[test]
    fn ring_outside() {
        let mut data = vec![0u8; 20 * 20 * 4];
        let mut canvas = Canvas::scaled(&mut data, 20, 20, 1.0);
        focus_ring(&mut canvas, 5, 5, 10, 10);
        let alpha = |x: usize, y: usize| data[(y * 20 + x) * 4 + 3];
        assert_eq!(alpha(10, 1), 255, "above, 2 px out");
        assert_eq!(alpha(18, 10), 255, "right, 2 px out");
        assert_eq!(alpha(10, 3), 0, "a gap between the ring and the control");
        assert_eq!(alpha(10, 5), 0, "the inside is left alone");
    }
}
