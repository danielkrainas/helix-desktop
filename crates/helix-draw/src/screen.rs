//! What the lock screen and the login screen share: the type scale, the
//! spacing scale and drawing helpers for their pieces.

use crate::{Canvas, Style, Text};

#[cfg(test)]
mod shape_tests {
    use crate::Canvas;

    #[test]
    fn clipping() {
        let mut data = vec![0u8; 20 * 20 * 4];
        let mut canvas = Canvas::scaled(&mut data, 20, 20, 1.0);
        canvas.clip(5, 5, 10, 10);
        canvas.rect(0, 0, 20, 20, 0xFFFFFFFF);
        canvas.unclip();
        let alpha = |x: usize, y: usize| data[(y * 20 + x) * 4 + 3];
        assert_eq!((alpha(4, 10), alpha(5, 10), alpha(14, 14), alpha(15, 10)), (0, 255, 255, 0));
    }

    #[test]
    fn arcs_and_outlines() {
        let mut data = vec![0u8; 40 * 40 * 4];
        let mut canvas = Canvas::scaled(&mut data, 40, 40, 1.0);
        // A quarter, from the right going down.
        canvas.arc(20.0, 20.0, 15.0, 3.0, 0.0, std::f64::consts::FRAC_PI_2, 0xFFFFFFFF);
        let alpha = |d: &[u8], x: usize, y: usize| d[(y * 40 + x) * 4 + 3];
        assert!(alpha(&data, 30, 30) > 200, "on the arc (45°)");
        assert_eq!(alpha(&data, 10, 10), 0, "the opposite quarter");
        let mut data = vec![0u8; 40 * 40 * 4];
        let mut canvas = Canvas::scaled(&mut data, 40, 40, 1.0);
        canvas.round_outline(10.0, 10.0, 20.0, 20.0, 6.0, 2.0, 0xFFFFFFFF);
        assert!(alpha(&data, 20, 9) > 200, "just outside the top edge");
        assert_eq!(alpha(&data, 20, 20), 0, "hollow");
        assert_eq!(alpha(&data, 20, 11), 0, "nothing inside the edge");
    }
}

/// Text sizes (logical px): a big clock's size comes from
/// [`display_px`]; then titles (a user's name), body text and captions.
pub const TITLE: f32 = 22.0;
pub const BODY: f32 = 15.0;
pub const CAPTION: f32 = 13.0;
/// A date or subtitle under a big clock.
pub const SUBTITLE: f32 = 21.0;

/// Spacing between pieces, smallest to largest.
pub const SPACE: [i32; 7] = [4, 8, 12, 16, 24, 32, 48];

/// A big clock's size on a monitor `h` logical px tall: about 9% of it,
/// from 64 to 128.
pub fn display_px(h: f64) -> f32 {
    (h * 0.09).clamp(64.0, 128.0) as f32
}

/// A face of the system font at a weight: `weight` is fontconfig's name
/// ("light", "semibold", "bold"); `None` when fontconfig has no face of
/// that weight (it falls back to another weight).
pub fn load_weight(weight: &str) -> Option<Text> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}\n%{style}", &format!("sans-serif:{weight}")]).output().ok().filter(|o| o.status.success())?;
    let out = String::from_utf8_lossy(&out.stdout).into_owned();
    let (file, style) = out.split_once('\n')?;
    let wanted = weight.replace('-', "").to_lowercase();
    style.to_lowercase().replace([' ', '-'], "").contains(&wanted).then(|| Text::load_file(file)).flatten()
}

/// The face for a big clock: light if the system font has one, else
/// bold, else `regular`.
pub fn display_face() -> Option<Text> {
    load_weight("light").or_else(|| load_weight("bold"))
}

/// Width of `s` with every digit as wide as the widest digit.
pub fn tabular_width(text: &Text, s: &str, px: f32) -> f32 {
    let digit = digit_width(text, px);
    s.chars().map(|c| if c.is_ascii_digit() { digit } else { text.width(&c.to_string(), px) }).sum()
}

fn digit_width(text: &Text, px: f32) -> f32 {
    ('0'..='9').map(|c| text.width(&c.to_string(), px)).fold(0.0, f32::max)
}

/// Draw `s` from `x` (baseline `y`) with tabular digits: each digit
/// centered in a cell as wide as the widest, so a clock does not jiggle
/// when it changes.
pub fn draw_tabular(canvas: &mut Canvas, text: &Text, s: &str, x: f32, y: f32, px: f32, color: u32) {
    let digit = digit_width(text, px);
    let mut pen = x;
    for c in s.chars() {
        let glyph = c.to_string();
        let w = text.width(&glyph, px);
        if c.is_ascii_digit() {
            text.draw(canvas, &glyph, pen + (digit - w) / 2.0, y, Style { px, color, max_width: f32::MAX });
            pen += digit;
        } else {
            text.draw(canvas, &glyph, pen, y, Style { px, color, max_width: f32::MAX });
            pen += w;
        }
    }
}

/// Colors of initials avatars.
pub const AVATAR_COLORS: [u32; 7] = [0xFF1C71D8, 0xFF26A269, 0xFFC64600, 0xFF813D9C, 0xFFA51D2D, 0xFF865E3C, 0xFF5E5C64];

/// The color of `name`'s initials avatar: always the same for a name.
pub fn avatar_color(name: &str) -> u32 {
    let hash = name.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    AVATAR_COLORS[hash as usize % AVATAR_COLORS.len()]
}

/// Up to two initials: of the first and last words ("Ann Smith": "AS",
/// "danny": "D").
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().filter(|w| w.chars().next().is_some_and(char::is_alphanumeric)).collect();
    let first = |w: &str| w.chars().next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default();
    match words.as_slice() {
        [] => String::new(),
        [one] => first(one),
        [a, .., b] => first(a) + &first(b),
    }
}

/// A user's round picture of radius `r` centered at (cx, cy): `picture`
/// cropped to a circle, else `name`'s initials (in `bold`, else `text`)
/// on a color picked from the name. High contrast: initials white on
/// black with a white ring.
#[allow(clippy::too_many_arguments)]
pub fn avatar(canvas: &mut Canvas, text: Option<&Text>, bold: Option<&Text>, name: &str, picture: Option<&crate::Image>, cx: f64, cy: f64, r: f64, opacity: f64) {
    if let Some(picture) = picture {
        canvas.image_rounded(cx - r, cy - r, 2.0 * r, 2.0 * r, r, picture, opacity as f32);
        return;
    }
    if crate::high_contrast() {
        canvas.circle(cx, cy, r, fade(0xFF000000, opacity));
        canvas.ring(cx, cy, r - 0.5, 1.0, fade(0xFFFFFFFF, opacity));
    } else {
        canvas.circle(cx, cy, r, fade(avatar_color(name), opacity));
    }
    let Some(face) = bold.or(text) else { return };
    let letters = initials(name);
    let px = (r * if letters.chars().count() > 1 { 0.78 } else { 0.9 }) as f32;
    let w = face.width(&letters, px);
    let cap = face.ascent(px) * 0.72;
    face.draw(canvas, &letters, cx as f32 - w / 2.0, cy as f32 + cap / 2.0, Style { px, color: fade(0xFFFFFFFF, opacity), max_width: f32::MAX });
}

/// The accent (focus rings), as chosen (`crate::accent`).
pub fn accent() -> u32 {
    crate::accent::current().color
}
/// Errors (a field's ring after a failed check).
pub const ERROR: u32 = 0xFFE01B24;
/// Destructive buttons' fill.
pub const DESTRUCTIVE: u32 = 0xFFC01C28;
/// Text on the screens.
pub const FG: u32 = 0xFFEDEDEF;
/// Placeholders and disabled things on an [`entry`].
pub const PLACEHOLDER: u32 = 0xFFA9A9B2;

/// A password entry's height and corner radius (logical px).
pub const ENTRY_H: f64 = 48.0;
pub const ENTRY_RADIUS: f64 = 12.0;
/// Hidden characters: dots this wide, this far apart.
const DOT: f64 = 8.0;
const DOT_PITCH: f64 = 16.0;
const ENTRY_PAD: f64 = 16.0;
/// The round button at an entry's right end.
const BUTTON_D: f64 = 36.0;

/// What sits at the right end of an [`entry`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Trailing {
    None,
    /// A round "go" button with an arrow (accent when `enabled`).
    Go { enabled: bool, hovered: bool },
    /// A spinner: its angle, or `None` standing still (no motion).
    Busy(Option<f64>),
}

/// A password entry (helix-desktop-spec `components/README.md`, "Text
/// fields"): rounded, an accent ring while it takes typing, a faint
/// outline otherwise; dots for the characters, a caret after them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Entry<'a> {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    /// Taking typing: the ring and the caret.
    pub focused: bool,
    /// The ring's color while focused (or [`ERROR`] after a failure,
    /// focused or not).
    pub ring: u32,
    /// Characters typed (dots), and the dots' opacity (dimmed while
    /// checking).
    pub dots: usize,
    pub dots_alpha: f64,
    pub placeholder: &'a str,
    pub trailing: Trailing,
    /// Room kept free left of the trailing piece (an indicator drawn by
    /// the caller), logical px.
    pub reserve: f64,
    /// Everything's opacity.
    pub alpha: f64,
}

/// Where an entry's pieces went.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EntryPlaced {
    /// The trailing button's target (larger than its circle).
    pub button: Option<(f64, f64, f64, f64)>,
    /// The center of the trailing piece, and the free room's right edge.
    pub trailing_center: (f64, f64),
    pub room_end: f64,
}

/// Draw `e`; `text` writes the placeholder.
pub fn entry(canvas: &mut Canvas, text: Option<&Text>, e: &Entry) -> EntryPlaced {
    let (x, y, w, h) = (e.x, e.y, e.w, ENTRY_H);
    let hc = crate::high_contrast();
    let a = e.alpha;
    // Fill, then the ring or outline around it.
    canvas.round_rect(x, y, w, h, ENTRY_RADIUS, fade(if hc { 0xFF000000 } else { 0x26FFFFFF }, a));
    let error = e.ring == ERROR;
    if e.focused || error {
        let ring = if hc && !error { 0xFFFFFFFF } else { e.ring };
        canvas.round_outline(x, y, w, h, ENTRY_RADIUS, 2.0, fade(ring, a));
    } else {
        canvas.round_outline(x, y, w, h, ENTRY_RADIUS, 1.0, fade(if hc { 0xFFFFFFFF } else { 0x40FFFFFF }, a));
    }
    let cy = y + h / 2.0;
    let center = (x + w - h / 2.0, cy);
    let mut placed = EntryPlaced { button: None, trailing_center: center, room_end: x + w - ENTRY_PAD };
    match e.trailing {
        Trailing::None => {}
        Trailing::Go { enabled, hovered } => {
            let accent = crate::accent::current();
            let fill = match (enabled, hovered) {
                (true, false) => accent.fill,
                (true, true) => accent.fill_hover,
                (false, _) => 0x1FFFFFFF,
            };
            canvas.circle(center.0, center.1, BUTTON_D / 2.0, fade(fill, a));
            arrow(canvas, center.0, center.1, fade(if enabled { accent.on_fill } else { PLACEHOLDER }, a));
            placed.button = Some((x + w - h, y, h, h));
            placed.room_end = center.0 - h / 2.0 - 4.0;
        }
        Trailing::Busy(angle) => {
            spinner(canvas, center.0, center.1, 9.0, angle, fade(FG, a));
            placed.room_end = center.0 - h / 2.0 - 4.0;
        }
    }
    placed.room_end -= e.reserve;
    // Dots, as many as fit, then the caret.
    let room = placed.room_end - (x + ENTRY_PAD) - 8.0;
    let fit = ((room - DOT) / DOT_PITCH).floor().max(0.0) as usize + 1;
    let count = e.dots.min(fit);
    for i in 0..count {
        canvas.circle(x + ENTRY_PAD + DOT / 2.0 + i as f64 * DOT_PITCH, cy, DOT / 2.0, fade(FG, a * e.dots_alpha));
    }
    let caret_x = if count == 0 { x + ENTRY_PAD } else { x + ENTRY_PAD + DOT + (count - 1) as f64 * DOT_PITCH + 5.0 };
    if count == 0 {
        if let Some(t) = text {
            let px = BODY + 2.0;
            let cap = t.ascent(px) * 0.72;
            let lx = (caret_x + 8.0) as f32;
            t.draw(canvas, e.placeholder, lx, (cy + f64::from(cap) / 2.0) as f32, Style { px, color: fade(PLACEHOLDER, a), max_width: (placed.room_end as f32 - lx).max(0.0) });
        }
    }
    if e.focused {
        let ch = 22.0;
        canvas.round_rect(caret_x, cy - ch / 2.0, 2.0, ch, 1.0, fade(FG, a));
    }
    placed
}

/// A right arrow ("go") centered at (cx, cy), 14 logical px wide.
pub fn arrow(canvas: &mut Canvas, cx: f64, cy: f64, color: u32) {
    let (x0, x1) = (cx - 7.0, cx + 7.0);
    canvas.line((x0, cy), (x1 - 0.8, cy), 2.2, color);
    canvas.line((x1 - 6.0, cy - 6.0), (x1, cy), 2.2, color);
    canvas.line((x1 - 6.0, cy + 6.0), (x1, cy), 2.2, color);
}

/// A spinner of radius `r` at (cx, cy): an arc turning with `angle`;
/// without one (no motion) a still, dotted ring.
pub fn spinner(canvas: &mut Canvas, cx: f64, cy: f64, r: f64, angle: Option<f64>, color: u32) {
    match angle {
        Some(angle) => {
            canvas.ring(cx, cy, r, 2.5, fade(color, 0.25));
            canvas.arc(cx, cy, r, 2.5, angle, std::f64::consts::PI * 0.6, color);
        }
        None => {
            for i in 0..8 {
                let t = f64::from(i) / 8.0 * std::f64::consts::TAU;
                canvas.circle(cx + r * t.cos(), cy + r * t.sin(), 1.6, fade(color, 0.4 + 0.6 * f64::from(i) / 7.0));
            }
        }
    }
}

/// How a button looks at a moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ButtonState {
    pub hovered: bool,
    pub pressed: bool,
    /// Has the keyboard focus: a ring around it.
    pub focused: bool,
}

/// What a button does, for its colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// Translucent light.
    Normal,
    /// The accent.
    Suggested,
    /// Red: destroys or ends something.
    Destructive,
}

fn button_fill(kind: ButtonKind, state: ButtonState) -> u32 {
    match (kind, state.pressed, state.hovered) {
        (ButtonKind::Normal, true, _) => 0x59FFFFFF,
        (ButtonKind::Normal, false, true) => 0x40FFFFFF,
        (ButtonKind::Normal, false, false) => 0x26FFFFFF,
        // The accent's fills: its label color keeps 4.5:1 on them.
        (ButtonKind::Suggested, true, _) => crate::accent::current().fill_pressed,
        (ButtonKind::Suggested, false, true) => crate::accent::current().fill_hover,
        (ButtonKind::Suggested, false, false) => crate::accent::current().fill,
        // Darker than ERROR: white text on it keeps 4.5:1.
        (ButtonKind::Destructive, true, _) => 0xFFA51D2D,
        (ButtonKind::Destructive, false, true) => 0xFFC92A35,
        (ButtonKind::Destructive, false, false) => DESTRUCTIVE,
    }
}

/// A round icon button of diameter `d` at (cx, cy). High contrast: black
/// with a white border (white while pressed or hovered).
#[allow(clippy::too_many_arguments)]
pub fn round_button(canvas: &mut Canvas, cx: f64, cy: f64, d: f64, icon: Option<&crate::Image>, icon_px: f64, state: ButtonState, alpha: f64) {
    let r = d / 2.0;
    if crate::high_contrast() {
        canvas.circle(cx, cy, r, fade(if state.pressed || state.hovered { 0xFF404040 } else { 0xFF000000 }, alpha));
        canvas.ring(cx, cy, r - 0.5, 1.0, fade(0xFFFFFFFF, alpha));
    } else {
        canvas.circle(cx, cy, r, fade(button_fill(ButtonKind::Normal, state), alpha));
    }
    if let Some(icon) = icon {
        canvas.image_alpha(cx - icon_px / 2.0, cy - icon_px / 2.0, icon_px, icon_px, icon, alpha as f32);
    }
    if state.focused {
        canvas.ring(cx, cy, r + 3.0, 2.0, fade(accent(), alpha));
    }
}

/// A rounded push button with `label` centered, at (x, y, w, h).
#[allow(clippy::too_many_arguments)]
pub fn push_button(canvas: &mut Canvas, text: Option<&Text>, (x, y, w, h): (f64, f64, f64, f64), label: &str, kind: ButtonKind, state: ButtonState, alpha: f64) {
    let radius = 10.0;
    if crate::high_contrast() {
        let fill = if state.pressed || state.hovered { 0xFF404040 } else { 0xFF000000 };
        canvas.round_rect(x, y, w, h, radius, fade(fill, alpha));
        canvas.round_outline(x + 1.0, y + 1.0, w - 2.0, h - 2.0, radius - 1.0, 1.0, fade(0xFFFFFFFF, alpha));
    } else {
        canvas.round_rect(x, y, w, h, radius, fade(button_fill(kind, state), alpha));
    }
    if state.focused {
        let ring = if kind == ButtonKind::Normal || crate::high_contrast() { accent() } else { FG };
        canvas.round_outline(x - 2.0, y - 2.0, w + 4.0, h + 4.0, radius + 2.0, 2.0, fade(ring, alpha));
    }
    if let Some(t) = text {
        let px = BODY;
        let lw = t.width(label, px).min(w as f32 - 16.0);
        let cap = t.ascent(px) * 0.72;
        t.draw(canvas, label, x as f32 + (w as f32 - lw) / 2.0, (y + h / 2.0) as f32 + cap / 2.0, Style { px, color: fade(button_text(kind), alpha), max_width: w as f32 - 16.0 });
    }
}

/// The label color on a button of `kind` (dark on a light accent).
fn button_text(kind: ButtonKind) -> u32 {
    if kind == ButtonKind::Suggested && !crate::high_contrast() { crate::accent::current().on_fill } else { FG }
}

/// `color` with its opacity multiplied by `alpha` (0 to 1).
pub fn fade(color: u32, alpha: f64) -> u32 {
    let a = (f64::from(color >> 24) * alpha.clamp(0.0, 1.0)).round() as u32;
    (a << 24) | (color & 0x00FF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales() {
        assert_eq!(display_px(1080.0), 97.2_f64 as f32);
        assert_eq!(display_px(600.0), 64.0);
        assert_eq!(display_px(2160.0), 128.0);
        assert_eq!(fade(0xFFEDEDEF, 0.5), 0x80EDEDEF);
        assert_eq!(fade(0x80000000, 0.5), 0x40000000);
    }

    #[test]
    fn avatars() {
        assert_eq!(initials("Ann Smith"), "AS");
        assert_eq!(initials("Jean-Luc de la Cruz"), "JC");
        assert_eq!(initials("danny"), "D");
        assert_eq!(initials("  "), "");
        assert_eq!(avatar_color("ann"), avatar_color("ann"));
        // A picture is cropped to a circle: corners untouched, middle drawn.
        let picture = crate::Image { width: 2, height: 2, rgba: vec![255; 16] };
        let mut data = vec![0u8; 20 * 20 * 4];
        let mut canvas = Canvas::scaled(&mut data, 20, 20, 1.0);
        avatar(&mut canvas, None, None, "ann", Some(&picture), 10.0, 10.0, 10.0, 1.0);
        let alpha = |x: usize, y: usize| data[(y * 20 + x) * 4 + 3];
        assert_eq!(alpha(10, 10), 255);
        assert_eq!(alpha(0, 0), 0);
        assert!(alpha(10, 0) > 100, "the circle reaches the top edge");
    }

    #[test]
    fn button_text_contrast() {
        let hovered = ButtonState { hovered: true, ..Default::default() };
        for fill in [DESTRUCTIVE, button_fill(ButtonKind::Destructive, hovered)] {
            assert!(crate::backdrop::contrast(FG, fill) >= 4.5, "{fill:08x}: {}", crate::backdrop::contrast(FG, fill));
        }
        let suggested = [ButtonState::default(), hovered, ButtonState { pressed: true, ..Default::default() }].map(|state| button_fill(ButtonKind::Suggested, state));
        for fill in suggested {
            let c = crate::backdrop::contrast(button_text(ButtonKind::Suggested), fill);
            assert!(c >= 4.5, "{fill:08x}: {c:.2}");
        }
    }

    #[test]
    fn entries() {
        for scale in [1.0, 2.0] {
            let (w, h) = (300usize, 60usize);
            let pw = (w as f64 * scale) as usize;
            let mut data = vec![0u8; pw * (h as f64 * scale) as usize * 4];
            let mut canvas = Canvas::scaled(&mut data, w, h, scale);
            let e = Entry { x: 10.0, y: 6.0, w: 280.0, focused: true, ring: accent(), dots: 40, dots_alpha: 1.0, placeholder: "Password", trailing: Trailing::Go { enabled: true, hovered: false }, reserve: 0.0, alpha: 1.0 };
            let placed = entry(&mut canvas, None, &e);
            let (bx, _, bw, _) = placed.button.expect("a button");
            assert!(bw >= 44.0 && bx + bw <= 290.5, "{:?}", placed.button);
            // Along the middle row: separate dots (dark gaps between), and
            // none under the button.
            let cy = ((6.0 + ENTRY_H / 2.0) * scale) as usize;
            let lit: Vec<bool> = (0..pw).map(|x| data[(cy * pw + x) * 4 + 1] > 200).collect();
            let runs = lit.windows(2).filter(|p| !p[0] && p[1]).count();
            assert!(runs > 10, "dots apart at scale {scale}: {runs}");
            let last_dot = (0..pw).rev().find(|&x| lit[x] && (x as f64) < (bx - 4.0) * scale).unwrap();
            assert!((last_dot as f64) < placed.room_end * scale + 1.0, "dots stop before the button");
        }
    }

    #[test]
    fn tabular_digits() {
        let Some(text) = Text::load() else { return };
        assert_eq!(tabular_width(&text, "11:11", 40.0), tabular_width(&text, "10:08", 40.0));
    }
}
