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
/// A notification (a row with secondary text on it) under the pointer:
/// lighter than a popover only as far as Secondary text keeps 4.5:1 on it.
pub const CARD_HOVER: u32 = 0xFF3A3A42;
/// What dims the screen behind a dialog.
pub const SCRIM: u32 = 0xB00C0C10;
/// A popover's shadow at its darkest (just outside the box), how far it
/// reaches and how much lower than the box it falls.
pub const SHADOW: u32 = 0x60000000;
pub const SHADOW_SPREAD: f64 = 12.0;
pub const SHADOW_DROP: f64 = 2.0;

// ----- Shape and space -----------------------------------------------------

/// Corner radii (logical px): popovers, dialogs, banners, the switcher,
/// the screenshot toolbar.
pub const RADIUS_POPOVER: f64 = 12.0;
/// Cards and pictures: notifications in the list, window pictures, tiles.
pub const RADIUS_CARD: f64 = 12.0;
/// Buttons, switches, fields, wells, keys.
pub const RADIUS_BUTTON: f64 = 8.0;
/// Rows inside a popover or list (menu entries, results, list rows).
pub const RADIUS_ROW: f64 = 6.0;
/// Tooltips.
pub const RADIUS_TOOLTIP: f64 = 6.0;
/// Check boxes.
pub const RADIUS_CHECK: f64 = 4.0;

/// The radius that makes a box `h` tall a pill (or a circle when square).
pub fn pill(h: f64) -> f64 {
    h / 2.0
}

/// Spacing between pieces, smallest to largest (logical px): 4, 8, 12,
/// 16, 24, 32, 48.
pub const SPACE: [i32; 7] = crate::screen::SPACE;
/// How far rows sit inside a popover's edge (so their rounded highlight
/// clears its corners).
pub const POPOVER_PAD: i32 = 6;

// ----- Type ----------------------------------------------------------------

/// The shell's text sizes (logical px, before the text scale): every size
/// the shell draws text at is one of these. Words the user has to read are
/// at least [`size::CAPTION`].
pub mod size {
    /// Counts on a badge and week numbers: glanced at, never sentences.
    pub const TINY: f32 = 11.0;
    /// Secondary lines: app names, times, percentages, subtitles, hints.
    pub const CAPTION: f32 = 13.0;
    /// Rows and controls: menu entries, the panel, list rows, toggles.
    pub const LABEL: f32 = 14.0;
    /// Body text: messages, results, buttons, titles of rows.
    pub const BODY: f32 = 15.0;
    /// Headings, in the bold face: notification summaries, group headers,
    /// quick settings' toggle titles.
    pub const HEADING: f32 = 15.0;
    /// What is typed in a search field, a folder's name.
    pub const ENTRY: f32 = 17.0;
    /// Titles of dialogs and popovers, the calendar's date (bold).
    pub const TITLE: f32 = 20.0;
    /// Big titles (the break screen's), bold.
    pub const LARGE_TITLE: f32 = 30.0;
}

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

/// A popover or dialog box at (x, y, w, h): rounded ([`RADIUS_POPOVER`]),
/// a 1 px edge outside it and a soft shadow under it (high contrast: black
/// with a 2 px white edge, no shadow).
pub fn popover(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32) {
    popover_round(canvas, f64::from(x), f64::from(y), f64::from(w), f64::from(h), RADIUS_POPOVER);
}

/// [`popover`] with corners of `radius` (a pill-shaped OSD, a tooltip).
pub fn popover_round(canvas: &mut Canvas, x: f64, y: f64, w: f64, h: f64, radius: f64) {
    if crate::high_contrast() {
        canvas.round_rect(x, y, w, h, radius, BLACK);
        canvas.round_outline(x, y, w, h, radius, 2.0, WHITE);
        return;
    }
    shadow(canvas, x, y, w, h, radius);
    canvas.round_rect(x - 1.0, y - 1.0, w + 2.0, h + 2.0, radius + 1.0, BORDER);
    canvas.round_rect(x, y, w, h, radius, POPOVER);
}

/// The soft shadow of a box at (x, y, w, h) with corners of `radius`:
/// [`SHADOW`] fading out over [`SHADOW_SPREAD`] px around it, a little
/// lower than the box. None in high contrast (edges are lines there).
pub fn shadow(canvas: &mut Canvas, x: f64, y: f64, w: f64, h: f64, radius: f64) {
    if crate::high_contrast() {
        return;
    }
    canvas.shadow(x, y + SHADOW_DROP, w, h, radius, SHADOW_SPREAD, SHADOW);
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

/// A control (a switch, a choice, a button, a key) at (x, y, w, h),
/// rounded ([`RADIUS_BUTTON`]). High contrast: on is the accent, off
/// black, both with a white outline (2 px while hovered or pressed).
pub fn control(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32, look: Look) {
    control_round(canvas, (f64::from(x), f64::from(y), f64::from(w), f64::from(h)), RADIUS_BUTTON, look);
}

/// [`control`] with corners of `radius` ([`pill`] for a pill).
pub fn control_round(canvas: &mut Canvas, rect: (f64, f64, f64, f64), radius: f64, look: Look) {
    control_in(canvas, rect, radius, look, crate::high_contrast());
}

fn control_in(canvas: &mut Canvas, (x, y, w, h): (f64, f64, f64, f64), radius: f64, look: Look, hc: bool) {
    if hc {
        canvas.round_rect(x, y, w, h, radius, if look.on { crate::accent::Accent::Blue.palette().fill } else { BLACK });
        let width = if look.hovered || look.pressed { 2.0 } else { 1.0 };
        inner_outline(canvas, (x, y, w, h), radius, width, WHITE);
        return;
    }
    canvas.round_rect(x, y, w, h, radius, control_fill(look));
}

/// A `width` px outline just inside the rounded box (x, y, w, h).
pub fn inner_outline(canvas: &mut Canvas, (x, y, w, h): (f64, f64, f64, f64), radius: f64, width: f64, color: u32) {
    canvas.round_outline(x + width, y + width, w - 2.0 * width, h - 2.0 * width, (radius - width).max(0.0), width, color);
}

/// A flat control: nothing at rest, the hovered or pressed fill under the
/// pointer (an icon button on a popover, a close button), the accent fill
/// when on. High contrast: a white outline while hovered or pressed.
pub fn flat(canvas: &mut Canvas, rect: (f64, f64, f64, f64), radius: f64, look: Look) {
    if look.on {
        control_round(canvas, rect, radius, look);
        return;
    }
    if !(look.hovered || look.pressed) {
        return;
    }
    if crate::high_contrast() {
        inner_outline(canvas, rect, radius, if look.pressed { 2.0 } else { 1.0 }, WHITE);
        return;
    }
    let (x, y, w, h) = rect;
    canvas.round_rect(x, y, w, h, radius, control_fill(look));
}

/// A row (a menu entry, a list row, a search result) in `look`: nothing
/// at rest, Hovered under the pointer or the keyboard's selection,
/// Pressed while pressed, the accent fill when on; rounded
/// ([`RADIUS_ROW`]). High contrast: a 2 px white outline (3 px pressed).
pub fn row(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32, look: Look) {
    let rect = (f64::from(x), f64::from(y), f64::from(w), f64::from(h));
    if look.on {
        control_round(canvas, rect, RADIUS_ROW, look);
        return;
    }
    if !(look.hovered || look.pressed) {
        return;
    }
    if crate::high_contrast() {
        inner_outline(canvas, rect, RADIUS_ROW, if look.pressed { 3.0 } else { 2.0 }, WHITE);
        return;
    }
    canvas.round_rect(rect.0, rect.1, rect.2, rect.3, RADIUS_ROW, if look.pressed { CONTROL_PRESSED } else { CONTROL_HOVER });
}

/// The highlight of a hovered or keyboard-selected row (see [`row`]).
pub fn row_highlight(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32) {
    row(canvas, x, y, w, h, Look { hovered: true, ..Default::default() });
}

/// A list's well at (x, y, w, h), rounded (outlined in high contrast).
pub fn well(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32) {
    let rect = (f64::from(x), f64::from(y), f64::from(w), f64::from(h));
    let hc = crate::high_contrast();
    canvas.round_rect(rect.0, rect.1, rect.2, rect.3, RADIUS_BUTTON, if hc { BLACK } else { WELL });
    if hc {
        inner_outline(canvas, rect, RADIUS_BUTTON, 1.0, WHITE);
    }
}

/// What a push button does, for its colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Control: the usual button.
    Normal,
    /// The accent fill: the answer most people want.
    Suggested,
    /// Red: destroys or ends something.
    Destructive,
    /// Nothing at rest; lit under the pointer.
    Flat,
}

/// A destructive button's fills (at rest, hovered, pressed): Text keeps
/// 4.5:1 on each.
const DESTRUCTIVE: [u32; 3] = [crate::screen::DESTRUCTIVE, 0xFFC92A35, 0xFFA51D2D];

/// The fill of a push button of `kind` in `look` (`None`: nothing drawn).
fn button_fill(kind: Kind, look: Look) -> Option<u32> {
    let pick = |[rest, hover, press]: [u32; 3]| if look.pressed { press } else if look.hovered { hover } else { rest };
    match kind {
        Kind::Normal => Some(control_fill(Look { on: false, ..look })),
        Kind::Suggested => Some(control_fill(Look { on: true, ..look })),
        Kind::Destructive => Some(pick(DESTRUCTIVE)),
        Kind::Flat if look.hovered || look.pressed => Some(control_fill(Look { on: false, ..look })),
        Kind::Flat => None,
    }
}

/// The text color on a push button of `kind`.
pub fn button_text(kind: Kind) -> u32 {
    if kind == Kind::Suggested && !crate::high_contrast() { on_accent() } else { TEXT }
}

/// A push button at (x, y, w, h) with `label` centered (in `text`, at
/// [`size::BODY`]): rounded ([`RADIUS_BUTTON`]), filled by `kind` and
/// `look`, the focus ring when `focused`. High contrast: black (Suggested:
/// the accent) with a white outline, 2 px while hovered or pressed.
#[allow(clippy::too_many_arguments)]
pub fn button(canvas: &mut Canvas, text: Option<&crate::Text>, (x, y, w, h): (i32, i32, i32, i32), label: &str, kind: Kind, look: Look, focused: bool) {
    let rect = (f64::from(x), f64::from(y), f64::from(w), f64::from(h));
    if crate::high_contrast() {
        if kind != Kind::Flat || look.hovered || look.pressed {
            control_round(canvas, rect, RADIUS_BUTTON, Look { on: kind == Kind::Suggested, ..look });
        }
    } else if let Some(fill) = button_fill(kind, look) {
        canvas.round_rect(rect.0, rect.1, rect.2, rect.3, RADIUS_BUTTON, fill);
    }
    if let Some(t) = text {
        let px = size::BODY;
        let room = (w - 16).max(0) as f32;
        let lw = t.width(label, px).min(room);
        let cap = t.ascent(px) * 0.72;
        t.draw(canvas, label, x as f32 + (w as f32 - lw) / 2.0, y as f32 + (h as f32 + cap) / 2.0, crate::Style { px, color: button_text(kind), max_width: room });
    }
    if focused {
        focus_ring_round(canvas, rect.0, rect.1, rect.2, rect.3, RADIUS_BUTTON);
    }
}

/// A switch (on/off) at (x, y): a pill [`SWITCH_W`]×[`SWITCH_H`], the
/// accent fill when on (Track when off) with a round knob at that end.
/// High contrast: outlined in white.
pub fn switch(canvas: &mut Canvas, x: f64, y: f64, on: bool, look: Look) {
    let (w, h) = (SWITCH_W, SWITCH_H);
    let hc = crate::high_contrast();
    let fill = if on {
        if hc { crate::accent::Accent::Blue.palette().fill } else { control_fill(Look { on: true, ..look }) }
    } else if hc {
        BLACK
    } else if look.hovered || look.pressed {
        CONTROL_PRESSED
    } else {
        TRACK
    };
    canvas.round_rect(x, y, w, h, h / 2.0, fill);
    if hc {
        inner_outline(canvas, (x, y, w, h), h / 2.0, if look.hovered || look.pressed { 2.0 } else { 1.0 }, WHITE);
    }
    let r = h / 2.0 - 2.0;
    let kx = if on { x + w - h / 2.0 } else { x + h / 2.0 };
    canvas.circle(kx, y + h / 2.0, r, if hc && !on { WHITE } else { TEXT });
}

/// A switch's size (logical px).
pub const SWITCH_W: f64 = 40.0;
pub const SWITCH_H: f64 = 22.0;

/// A push button that cannot be used now (nothing chosen yet, busy):
/// Control with its words dimmed ([`TEXT_DIM`]; white at 60 % in high
/// contrast).
pub fn button_off(canvas: &mut Canvas, text: Option<&crate::Text>, (x, y, w, h): (i32, i32, i32, i32), label: &str) {
    control(canvas, x, y, w, h, Look::default());
    if let Some(t) = text {
        let px = size::BODY;
        let room = (w - 16).max(0) as f32;
        let lw = t.width(label, px).min(room);
        let cap = t.ascent(px) * 0.72;
        t.draw(canvas, label, x as f32 + (w as f32 - lw) / 2.0, y as f32 + (h as f32 + cap) / 2.0, crate::Style { px, color: disabled_text(), max_width: room });
    }
}

/// Words of what cannot be used now: dimmed, and in high contrast (where
/// all text is white) white at 60 % (still 7:1 on black).
pub fn disabled_text() -> u32 {
    if crate::high_contrast() { 0x99FF_FFFF } else { TEXT_DIM }
}

/// A round flat button of diameter `d` centered at (cx, cy) (a close
/// button, an icon button on a popover): lit under the pointer, the focus
/// ring when `focused`. The caller draws its symbol ([`close_symbol`], an
/// icon) on it.
pub fn round_flat(canvas: &mut Canvas, cx: f64, cy: f64, d: f64, look: Look, focused: bool) {
    let r = d / 2.0;
    flat(canvas, (cx - r, cy - r, d, d), r, look);
    if focused {
        focus_ring_round(canvas, cx - r, cy - r, d, d, r);
    }
}

/// A round button of diameter `d` at (cx, cy) filled even at rest
/// (Control; the accent fill when on): quick settings' icon buttons, a
/// close button over a picture.
pub fn round_control(canvas: &mut Canvas, cx: f64, cy: f64, d: f64, look: Look, focused: bool) {
    let r = d / 2.0;
    control_round(canvas, (cx - r, cy - r, d, d), r, look);
    if focused {
        focus_ring_round(canvas, cx - r, cy - r, d, d, r);
    }
}

/// The close symbol (an ×, Helix's own drawing for `window-close-symbolic`)
/// `size` px across, centered at (cx, cy).
pub fn close_symbol(canvas: &mut Canvas, cx: f64, cy: f64, size: f64, color: u32) {
    let h = size / 2.0 - 1.0;
    canvas.line((cx - h, cy - h), (cx + h, cy + h), 1.8, color);
    canvas.line((cx + h, cy - h), (cx - h, cy + h), 1.8, color);
}

/// Which way a chevron points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Towards {
    Left,
    Right,
    Up,
    Down,
}

/// A chevron `size` px across centered at (cx, cy), pointing `towards`
/// (on a mirrored canvas it is drawn mirrored, so "forward" stays
/// forward).
pub fn chevron(canvas: &mut Canvas, cx: f64, cy: f64, size: f64, towards: Towards, color: u32) {
    let (a, b) = (size / 2.0, size / 4.0);
    let points = match towards {
        Towards::Right => [(cx - b, cy - a), (cx + b, cy), (cx - b, cy + a)],
        Towards::Left => [(cx + b, cy - a), (cx - b, cy), (cx + b, cy + a)],
        Towards::Down => [(cx - a, cy - b), (cx, cy + b), (cx + a, cy - b)],
        Towards::Up => [(cx - a, cy + b), (cx, cy - b), (cx + a, cy + b)],
    };
    canvas.line(points[0], points[1], 1.8, color);
    canvas.line(points[1], points[2], 1.8, color);
}

/// A check box `size` px square at (x, y): rounded ([`RADIUS_CHECK`]),
/// the accent fill with a check mark when `checked`, Control with an
/// outline otherwise; hovered and pressed shades.
pub fn check_box(canvas: &mut Canvas, x: f64, y: f64, size: f64, checked: bool, look: Look) {
    let look = Look { on: checked, ..look };
    let rect = (x, y, size, size);
    control_round(canvas, rect, RADIUS_CHECK, look);
    if !checked && !crate::high_contrast() {
        inner_outline(canvas, rect, RADIUS_CHECK, 1.0, TEXT_DIM);
    }
    if checked {
        let c = control_text(look);
        let k = size / 14.0;
        canvas.line((x + 3.0 * k, y + 7.5 * k), (x + 6.0 * k, y + 10.5 * k), 1.8, c);
        canvas.line((x + 6.0 * k, y + 10.5 * k), (x + 11.0 * k, y + 3.5 * k), 1.8, c);
    }
}

/// A text field's box at (x, y, w, h) (authentication and secret
/// prompts, the launcher's and the overview's search): Control filled,
/// rounded ([`RADIUS_BUTTON`], or a pill when `pill_shaped`), the focus
/// ring while it takes typing and a faint outline otherwise (helix-desktop-spec
/// `components/README.md`, "Text fields"). High contrast: black with a
/// white outline.
pub fn field(canvas: &mut Canvas, (x, y, w, h): (i32, i32, i32, i32), focused: bool, pill_shaped: bool) {
    let rect = (f64::from(x), f64::from(y), f64::from(w), f64::from(h));
    let radius = if pill_shaped { pill(rect.3) } else { RADIUS_BUTTON };
    let hc = crate::high_contrast();
    canvas.round_rect(rect.0, rect.1, rect.2, rect.3, radius, if hc { BLACK } else { CONTROL });
    if focused {
        canvas.round_outline(rect.0, rect.1, rect.2, rect.3, radius, 2.0, focus());
    } else {
        inner_outline(canvas, rect, radius, 1.0, if hc { WHITE } else { 0x30FF_FFFF });
    }
}

/// A tooltip's box at (x, y, w, h): a small popover ([`RADIUS_TOOLTIP`]).
pub fn tooltip(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32) {
    popover_round(canvas, f64::from(x), f64::from(y), f64::from(w), f64::from(h), RADIUS_TOOLTIP);
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
            (CARD_HOVER, POPOVER, 1.18, "a hovered notification"),
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
        for fg in [TEXT, TEXT_DIM] {
            assert!(contrast(fg, CARD_HOVER) >= 4.5, "{fg:08x} on a hovered notification");
        }
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
        control_in(&mut canvas, (5.0, 5.0, 30.0, 20.0), RADIUS_BUTTON, Look::default(), true);
        let px = |x: usize, y: usize| u32::from_le_bytes(data[(y * 40 + x) * 4..][..4].try_into().unwrap());
        assert_eq!(px(15, 5), 0xFFFFFFFF, "an off control is outlined in white");
        assert_eq!(px(15, 15), 0xFF000000, "and black inside");
        assert_eq!(px(5, 5), 0, "rounded: the corner stays clear");
    }

    /// Popovers are rounded, edged and shadowed; the shadow fades out.
    #[test]
    fn popovers() {
        let mut data = vec![0u8; 100 * 100 * 4];
        let mut canvas = Canvas::scaled(&mut data, 100, 100, 1.0);
        popover(&mut canvas, 20, 20, 60, 60);
        let alpha = |x: usize, y: usize| data[(y * 100 + x) * 4 + 3];
        let px = |x: usize, y: usize| u32::from_le_bytes(data[(y * 100 + x) * 4..][..4].try_into().unwrap());
        assert_eq!(px(50, 50), POPOVER, "filled");
        assert!(alpha(20, 20) < 0x60, "the corner is rounded off (only the shadow there)");
        assert!(alpha(50, 84) > alpha(50, 90), "the shadow fades out");
        assert!(alpha(50, 84) > 0 && alpha(50, 95) == 0, "and ends");
        assert!(alpha(50, 83) > alpha(50, 16), "it falls a little lower than the box");
    }

    /// Push buttons' words reach 4.5:1 in every state; Flat buttons draw
    /// nothing at rest.
    #[test]
    fn buttons() {
        let looks = [Look::default(), Look { hovered: true, ..Default::default() }, Look { pressed: true, ..Default::default() }];
        for kind in [Kind::Normal, Kind::Suggested, Kind::Destructive, Kind::Flat] {
            for look in looks {
                let Some(fill) = button_fill(kind, look) else {
                    assert!(kind == Kind::Flat && look == Look::default());
                    continue;
                };
                let c = contrast(button_text(kind), fill);
                assert!(c >= 4.5, "{kind:?} {look:?}: {c:.2}:1");
            }
        }
        let mut data = vec![0u8; 40 * 40 * 4];
        let mut canvas = Canvas::scaled(&mut data, 40, 40, 1.0);
        button(&mut canvas, None, (0, 0, 40, 40), "", Kind::Flat, Look::default(), false);
        assert!(data.iter().all(|b| *b == 0));
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
