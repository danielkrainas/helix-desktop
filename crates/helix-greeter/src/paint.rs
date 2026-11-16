//! Painting the greeter: a pure function of the [`View`] into a
//! [`Canvas`] (helixdesktop/spec `components/greeter.md`, "What the
//! screen shows"). The look follows the lock screen's: a centered column,
//! text fields as in "Text fields" (`components/README.md`); buttons,
//! menus and keys are `helix_draw::theme`'s.
//!
//! High contrast (`helix_draw::high_contrast`) and the text scale
//! (`helix_draw::theme::text_scale`) are the process's: the background
//! is black, text white and everything outlined in white; heights and
//! gaps that hold text grow with the text.

use helix_draw::theme::{self, Kind, Look};
use helix_draw::{Canvas, Style, Text};

use crate::app::{Focus, Hit, Main, PowerView, Setting, View};
use crate::osk::{self, Action, Osk};
use crate::power::PowerAction;
use helix_i18n::tr;

const FG: u32 = 0xFFEDEDEF;
const DIM_FG: u32 = 0xFF9A9AA4;
const ERROR_FG: u32 = 0xFFFF8A80;
const FIELD_BG: u32 = 0xFF2C2C33;
const ROW_SELECTED: u32 = 0xFF30313B;
const ROW_HOVER: u32 = 0xFF292A33;
/// The background: a vertical gradient.
const BG_TOP: u32 = 0xFF262A3B;
const BG_BOTTOM: u32 = 0xFF0F1015;
/// High contrast's background and lines.
const BLACK: u32 = 0xFF000000;
const WHITE: u32 = 0xFFFFFFFF;

const ROW_H: i32 = 56;
const ROW_GAP: i32 = 6;
const FIELD_H: i32 = 44;
const BUTTON_H: i32 = 36;
const MARGIN: i32 = 24;
const MENU_ITEM_H: i32 = 36;
/// The on-screen keyboard: a key's height, the gap between keys, the
/// padding around them, and the room left above the bottom bar.
const KEY_H: i32 = 38;
const KEY_GAP: i32 = 6;
const KEY_PAD: i32 = 8;
const OSK_ABOVE_BAR: i32 = 16;

/// A logical rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= f64::from(self.x) && y >= f64::from(self.y) && x < f64::from(self.x + self.w) && y < f64::from(self.y + self.h)
    }

    fn grow(self, by: i32) -> Self {
        Self { x: self.x - by, y: self.y - by, w: self.w + 2 * by, h: self.h + 2 * by }
    }

    #[cfg(test)]
    pub fn overlaps(&self, o: &Rect) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }

    fn tuple(self) -> (i32, i32, i32, i32) {
        (self.x, self.y, self.w, self.h)
    }
}

/// Where clicks land: later entries are on top.
pub type Hits = Vec<(Rect, Hit)>;

/// The topmost target under (x, y).
pub fn hit_at(hits: &Hits, x: f64, y: f64) -> Option<Hit> {
    hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
}

fn hc() -> bool {
    helix_draw::high_contrast()
}

/// Text: white in high contrast.
fn fg() -> u32 {
    if hc() { WHITE } else { FG }
}

/// Secondary text: white in high contrast too.
fn dim_fg() -> u32 {
    if hc() { WHITE } else { DIM_FG }
}

/// A length that holds text, grown with the text scale.
fn g(v: i32) -> i32 {
    theme::grow(v)
}

fn lerp(a: u32, b: u32, t: f32) -> [u8; 3] {
    let c = |c: u32, shift: u32| ((c >> shift) & 0xFF) as f32;
    [16, 8, 0].map(|s| (c(a, s) + (c(b, s) - c(a, s)) * t).round() as u8)
}

/// The dark vertical gradient every monitor shows (black in high
/// contrast).
pub fn paint_background(canvas: &mut Canvas) {
    let (width, height) = (canvas.width, canvas.height);
    let hc = hc();
    for y in 0..height {
        let t = if height > 1 { y as f32 / (height - 1) as f32 } else { 0.0 };
        let [r, g, b] = if hc { [0, 0, 0] } else { lerp(BG_TOP, BG_BOTTOM, t) };
        let row = &mut canvas.data[y * width * 4..(y + 1) * width * 4];
        for px in row.as_chunks_mut::<4>().0 {
            *px = [b, g, r, 255];
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn label(canvas: &mut Canvas, text: Option<&Text>, s: &str, x: f32, base: f32, px: f32, color: u32, max_w: f32) {
    if let Some(text) = text {
        text.draw(canvas, s, x, base, Style { px, color, max_width: max_w });
    }
}

/// The width of `s` at `px` (with the text scale).
fn width(text: Option<&Text>, s: &str, px: f32) -> f32 {
    text.map_or(s.chars().count() as f32 * px * theme::text_scale() * 0.55, |t| t.width(s, px))
}

/// Baseline that vertically centers `px` text in a box.
fn baseline(text: Option<&Text>, top: i32, height: i32, px: f32) -> f32 {
    let cap = text.map_or(px * theme::text_scale() * 0.7, |t| t.ascent(px) * 0.72);
    top as f32 + (height as f32 + cap) / 2.0
}

fn centered(canvas: &mut Canvas, text: Option<&Text>, s: &str, w: i32, base: f32, px: f32, color: u32) {
    let max_w = (w - 2 * MARGIN) as f32;
    let x = ((w as f32 - width(text, s, px).min(max_w)) / 2.0).max(MARGIN as f32);
    label(canvas, text, s, x, base, px, color, max_w);
}

/// A 2-unit ring just outside `r`: keyboard focus (the accent; white and
/// 2 units further out in high contrast, clear of a white outline).
fn focus_ring(canvas: &mut Canvas, r: Rect) {
    let o = r.grow(if hc() { 4 } else { 2 });
    theme::outline(canvas, o.x, o.y, o.w, o.h, 2, theme::focus());
}

/// The focus ring of a rounded control (`theme::button`'s).
fn control_ring(canvas: &mut Canvas, r: Rect) {
    theme::focus_ring_round(canvas, f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h), theme::RADIUS_BUTTON);
}

/// A text field's box: an accent ring while it takes typing, a faint
/// outline otherwise (high contrast: black, outlined in white).
fn text_field(canvas: &mut Canvas, r: Rect, focused: bool) {
    let hc = hc();
    let (ring, width) = match (focused, hc) {
        (true, _) => (theme::focus(), 2),
        (false, true) => (WHITE, 1),
        (false, false) => (0x30FFFFFF, 1),
    };
    let o = r.grow(width);
    canvas.rect(o.x, o.y, o.w, o.h, ring);
    canvas.rect(r.x, r.y, r.w, r.h, if hc { BLACK } else { FIELD_BG });
}

/// The caret at `x`, the height of a line of `px` text in field `r`.
fn caret(canvas: &mut Canvas, r: Rect, x: i32, px: f32) {
    let h = (px * theme::text_scale() * 1.3).round() as i32;
    canvas.rect(x, r.y + (r.h - h) / 2, 2, h, fg());
}

/// The contents of a field: dots for hidden text, else the text (its end
/// when too long), a placeholder when empty; the caret when focused.
fn field_contents(canvas: &mut Canvas, text: Option<&Text>, field: Rect, value: &str, secret: bool, placeholder: &str, focused: bool) {
    const PX: f32 = 17.0;
    let inner = field.w - 34;
    let end = if value.is_empty() {
        label(canvas, text, placeholder, (field.x + 20) as f32, baseline(text, field.y, field.h, PX), PX, dim_fg(), inner as f32);
        field.x + 14
    } else if secret {
        // One dot per character (drawn: not every font has a bullet).
        let pitch = g(14);
        let count = value.chars().count().min((inner / pitch).max(0) as usize) as i32;
        let r = 5.0 * f64::from(theme::text_scale());
        for i in 0..count {
            canvas.circle(f64::from(field.x + 20 + i * pitch), f64::from(field.y + field.h / 2), r, fg());
        }
        field.x + 14 + count * pitch
    } else {
        // The end of a long answer stays in view.
        let mut shown: &str = value;
        while width(text, shown, PX) > inner as f32 {
            let mut chars = shown.chars();
            chars.next();
            shown = chars.as_str();
        }
        label(canvas, text, shown, (field.x + 16) as f32, baseline(text, field.y, field.h, PX), PX, fg(), f32::MAX);
        field.x + 18 + width(text, shown, PX).ceil() as i32
    };
    if focused {
        caret(canvas, field, end, PX);
    }
}

/// The accent (focus rings, check marks): from the greeter's
/// configuration (`helix_draw::accent`).
fn accent() -> u32 {
    helix_draw::theme::accent()
}

/// A round picture standing in for the user: their initial on a color
/// picked from their name.
fn avatar(canvas: &mut Canvas, text: Option<&Text>, name: &str, picture: Option<&helix_draw::Image>, cx: i32, cy: i32, r: i32) {
    // The lock screen's avatar: the same colors and initials, or the
    // user's picture.
    helix_draw::screen::avatar(canvas, text, None, name, picture, f64::from(cx), f64::from(cy), f64::from(r), 1.0);
}

/// A push button (`theme::button`); `strong` for the action of a
/// question.
#[allow(clippy::too_many_arguments)]
fn button(canvas: &mut Canvas, text: Option<&Text>, x: i32, y: i32, s: &str, strong: bool, hovered: bool, focused: bool) -> Rect {
    let w = width(text, s, theme::size::BODY).ceil() as i32 + 32;
    let r = Rect { x, y, w, h: g(BUTTON_H) };
    let kind = if strong { Kind::Suggested } else { Kind::Normal };
    theme::button(canvas, text, r.tuple(), s, kind, Look { hovered, ..Default::default() }, focused);
    r
}

/// Messages under the field, from baseline `y`; returns the y below them.
fn notes(canvas: &mut Canvas, text: Option<&Text>, view: &View, x: i32, y: i32, w: i32) -> i32 {
    let mut base = y;
    for note in &view.notes {
        let lines = text.map(|t| t.wrap(&note.text, 15.0, w as f32, 3)).unwrap_or_else(|| vec![note.text.clone()]);
        for line in lines {
            label(canvas, text, &line, x as f32, base as f32, 15.0, if note.error { ERROR_FG } else { dim_fg() }, w as f32);
            base += g(21);
        }
    }
    base
}

/// The accessibility symbol (a person with open arms) centered at
/// (cx, cy).
fn a11y_symbol(canvas: &mut Canvas, cx: f64, cy: f64, color: u32) {
    let s = f64::from(theme::text_scale());
    let p = |x: f64, y: f64| (cx + x * s, cy + y * s);
    canvas.circle(cx, cy - 6.5 * s, 2.3 * s, color);
    canvas.line(p(-7.0, -2.5), p(7.0, -2.5), 1.8 * s, color);
    canvas.line(p(0.0, -2.5), p(0.0, 2.5), 1.8 * s, color);
    canvas.line(p(0.0, 2.5), p(-4.5, 8.0), 1.8 * s, color);
    canvas.line(p(0.0, 2.5), p(4.5, 8.0), 1.8 * s, color);
}

/// Where the on-screen keyboard goes on a `w`×`h` screen: centered at
/// the bottom, above the bottom bar.
pub fn osk_rect(w: i32, h: i32) -> Rect {
    let oh = 4 * g(KEY_H) + 3 * KEY_GAP + 2 * KEY_PAD;
    let ow = (w - 2 * MARGIN).min(g(760)).max(0);
    let bar = h - g(BUTTON_H) - MARGIN;
    Rect { x: (w - ow) / 2, y: bar - g(OSK_ABOVE_BAR) - oh, w: ow, h: oh }
}

/// The keys' places in `r`, in [`Osk::keys`]' order.
pub fn osk_keys(osk: &Osk, r: Rect) -> Vec<Rect> {
    let kh = g(KEY_H);
    let unit = (r.w - 2 * KEY_PAD - 9 * KEY_GAP) as f32 / 10.0;
    let key_w = |width: f32| (width * unit + (width - 1.0) * KEY_GAP as f32).round() as i32;
    let mut out = Vec::new();
    for (i, row) in osk.rows().iter().enumerate() {
        let row_w: i32 = row.iter().map(|k| key_w(k.width)).sum::<i32>() + (row.len() as i32 - 1) * KEY_GAP;
        let mut x = r.x + (r.w - row_w) / 2;
        let y = r.y + KEY_PAD + i as i32 * (kh + KEY_GAP);
        for k in row {
            let w = key_w(k.width);
            out.push(Rect { x, y, w, h: kh });
            x += w + KEY_GAP;
        }
    }
    out
}

/// The symbol on Shift, Backspace or Return, centered at (cx, cy).
fn key_symbol(canvas: &mut Canvas, action: Action, cx: f64, cy: f64, color: u32) {
    let s = f64::from(theme::text_scale());
    let p = |x: f64, y: f64| (cx + x * s, cy + y * s);
    let w = 1.8 * s;
    match action {
        Action::Shift => {
            // An up arrow.
            canvas.line(p(0.0, -7.0), p(-6.0, 0.0), w, color);
            canvas.line(p(0.0, -7.0), p(6.0, 0.0), w, color);
            canvas.line(p(0.0, -7.0), p(0.0, 7.0), w, color);
        }
        Action::Backspace => {
            // A left arrow.
            canvas.line(p(-8.0, 0.0), p(8.0, 0.0), w, color);
            canvas.line(p(-8.0, 0.0), p(-3.0, -5.0), w, color);
            canvas.line(p(-8.0, 0.0), p(-3.0, 5.0), w, color);
        }
        Action::Return => {
            // Down, then left.
            canvas.line(p(7.0, -6.0), p(7.0, 2.0), w, color);
            canvas.line(p(7.0, 2.0), p(-7.0, 2.0), w, color);
            canvas.line(p(-7.0, 2.0), p(-3.0, -2.0), w, color);
            canvas.line(p(-7.0, 2.0), p(-3.0, 6.0), w, color);
        }
        _ => {}
    }
}

/// The on-screen keyboard in `r`.
fn paint_osk(canvas: &mut Canvas, text: Option<&Text>, osk: &Osk, r: Rect, hover: Option<Hit>, hits: &mut Hits) {
    theme::popover(canvas, r.x, r.y, r.w, r.h);
    for (n, (key, k)) in osk.keys().iter().zip(osk_keys(osk, r)).enumerate() {
        let look = Look { on: key.action == Action::Shift && osk.shift, hovered: hover == Some(Hit::OskKey(n)), pressed: false };
        theme::control(canvas, k.x, k.y, k.w, k.h, look);
        let color = theme::control_text(look);
        match osk::label(key.action) {
            Some(s) => {
                let px = if s.chars().count() > 1 { theme::size::LABEL } else { theme::size::ENTRY };
                let lw = width(text, &s, px).min((k.w - 4) as f32);
                label(canvas, text, &s, k.x as f32 + (k.w as f32 - lw) / 2.0, baseline(text, k.y, k.h, px), px, color, (k.w - 4) as f32);
            }
            None => key_symbol(canvas, key.action, f64::from(k.x) + f64::from(k.w) / 2.0, f64::from(k.y) + f64::from(k.h) / 2.0, color),
        }
        hits.push((k, Hit::OskKey(n)));
    }
}

/// A menu of `count` items `item_h` high, `mw` wide, right-aligned at
/// `right`, above `bottom`: the popover, and its items' places.
fn menu(canvas: &mut Canvas, right: i32, bottom: i32, mw: i32, count: usize, item_h: i32) -> Vec<Rect> {
    let mh = count as i32 * item_h + 2 * theme::POPOVER_PAD;
    let m = Rect { x: right - mw, y: (bottom - 8 - mh).max(MARGIN), w: mw, h: mh };
    theme::popover(canvas, m.x, m.y, m.w, m.h);
    (0..count)
        .map(|i| Rect { x: m.x + theme::POPOVER_PAD, y: m.y + theme::POPOVER_PAD + i as i32 * item_h, w: m.w - 2 * theme::POPOVER_PAD, h: item_h })
        .take_while(|item| item.y + item.h <= m.y + m.h)
        .collect()
}

/// Paint the primary monitor's greeter, `w`×`h` logical; returns where
/// clicks land.
/// Users' pictures by login name.
pub type Pictures = std::collections::HashMap<String, helix_draw::Image>;

#[cfg(test)]
pub fn paint(canvas: &mut Canvas, text: Option<&Text>, view: &View, w: i32, h: i32) -> Hits {
    paint_with(canvas, text, view, w, h, &Pictures::new())
}

/// [`paint`] with the users' `pictures`.
pub fn paint_with(canvas: &mut Canvas, text: Option<&Text>, view: &View, w: i32, h: i32, pictures: &Pictures) -> Hits {
    paint_background(canvas);
    let mut hits = Hits::new();
    let hovered = |hit: Hit| view.hover == Some(hit);
    let hc = hc();
    let osk_at = view.osk.map(|_| osk_rect(w, h));

    // Clock, date and banner (higher up with the keyboard shown).
    let clock_base = if osk_at.is_some() { g(64) + 12 } else { (h / 8).max(72) + 20 };
    centered(canvas, text, &view.time, w, clock_base as f32, 64.0, fg());
    centered(canvas, text, &view.date, w, (clock_base + g(36)) as f32, 18.0, dim_fg());
    let mut header_end = clock_base + g(36);
    if let Some(banner) = &view.banner {
        let lines = text.map(|t| t.wrap(banner, 15.0, (w - 2 * MARGIN).min(640) as f32, 3)).unwrap_or_default();
        for (i, line) in lines.iter().enumerate() {
            centered(canvas, text, line, w, (clock_base + g(70) + g(21) * i as i32) as f32, 15.0, fg());
        }
        header_end = clock_base + g(70) + g(21) * lines.len() as i32;
    }

    // The column.
    let col_w = 360.min(w - 2 * MARGIN).max(0);
    let col_x = (w - col_w) / 2;
    let bar_y = h - g(BUTTON_H) - MARGIN;
    let top = if osk_at.is_some() { header_end + g(16) } else { (h * 34 / 100).max(header_end + 40) };
    let limit = osk_at.map_or(bar_y, |o| o.y - 12);
    let main_focused = view.focus == Focus::Main;
    let notes_y = match &view.main {
        Main::Loading => top + 20,
        Main::Users { names, logins, selected } => {
            let row_h = g(ROW_H);
            let room = (limit - 24 - top).max(row_h);
            let visible = ((room + ROW_GAP) / (row_h + ROW_GAP)).max(1) as usize;
            let first = (selected + 1).saturating_sub(visible);
            let mut y = top;
            let r = g(20);
            for (i, name) in names.iter().enumerate().skip(first).take(visible) {
                let row = Rect { x: col_x, y, w: col_w, h: row_h };
                if i == *selected {
                    if main_focused {
                        focus_ring(canvas, row);
                    }
                    if hc {
                        theme::outline(canvas, row.x, row.y, row.w, row.h, 2, WHITE);
                    } else {
                        canvas.rect(row.x, row.y, row.w, row.h, ROW_SELECTED);
                    }
                } else if hovered(Hit::User(i)) {
                    if hc {
                        theme::outline(canvas, row.x, row.y, row.w, row.h, 1, WHITE);
                    } else {
                        canvas.rect(row.x, row.y, row.w, row.h, ROW_HOVER);
                    }
                }
                avatar(canvas, text, name, logins.get(i).and_then(|l| pictures.get(l)), col_x + 12 + r, y + row_h / 2, r);
                label(canvas, text, name, (col_x + 24 + 2 * r) as f32, baseline(text, y, row_h, 17.0), 17.0, fg(), (col_w - 36 - 2 * r) as f32);
                hits.push((row, Hit::User(i)));
                y += row_h + ROW_GAP;
            }
            // More users than fit: a hint at the cut-off edge.
            if first > 0 {
                canvas.rect(col_x + col_w / 2 - 12, top - 10, 24, 2, dim_fg());
            }
            if first + visible < names.len() {
                canvas.rect(col_x + col_w / 2 - 12, y + 2, 24, 2, dim_fg());
            }
            y + 24
        }
        Main::Username { text: typed } => {
            let field = Rect { x: col_x, y: top + 20, w: col_w, h: g(FIELD_H) };
            text_field(canvas, field, main_focused);
            field_contents(canvas, text, field, typed, false, tr("Username"), main_focused);
            hits.push((field, Hit::Field));
            field.y + field.h + g(30)
        }
        Main::Login { user, login, question, text: typed, busy } => {
            // With the keyboard shown there is no room for the picture.
            let field_y = if osk_at.is_some() {
                centered(canvas, text, user, w, (top + g(22)) as f32, 20.0, fg());
                top + g(64)
            } else {
                avatar(canvas, text, user, pictures.get(login), w / 2, top + g(36), g(36));
                centered(canvas, text, user, w, (top + g(104)) as f32, 20.0, fg());
                top + g(150)
            };
            let field = Rect { x: col_x, y: field_y, w: col_w, h: g(FIELD_H) };
            if let Some(q) = question {
                label(canvas, text, &q.prompt, col_x as f32, (field.y - 12) as f32, 15.0, dim_fg(), col_w as f32);
                // Waiting for greetd: no typing, so no ring or caret.
                let focused = main_focused && !busy;
                text_field(canvas, field, focused);
                field_contents(canvas, text, field, typed, q.secret, "", focused);
                hits.push((field, Hit::Field));
            }
            field.y + field.h + g(30)
        }
    };
    let mut below = notes(canvas, text, view, col_x, notes_y, col_w);
    if let Main::Login { .. } = view.main {
        below = below.max(notes_y) - 4;
        let back = button(canvas, text, col_x, below, tr("Back"), false, hovered(Hit::Back), false);
        hits.push((back, Hit::Back));
    }

    // The on-screen keyboard, above the bottom bar.
    if let (Some(osk), Some(r)) = (&view.osk, osk_at) {
        paint_osk(canvas, text, osk, r, view.hover, &mut hits);
    }

    // Power, bottom left.
    let y = bar_y;
    let power_hits = view.power.hits();
    let focus_power = |n: usize| view.focus == Focus::Power(n);
    match view.power {
        PowerView::Buttons(_) => {
            let mut x = MARGIN;
            for (n, hit) in power_hits.iter().enumerate() {
                let s = if *hit == Hit::Power(PowerAction::Restart) { tr("Restart…") } else { tr("Power Off…") };
                let r = button(canvas, text, x, y, s, false, hovered(*hit), focus_power(n));
                hits.push((r, *hit));
                x = r.x + r.w + 12;
            }
        }
        PowerView::Confirm(action) => {
            let (question, verb) = match action {
                PowerAction::Restart => (tr("Restart this computer?"), tr("Restart")),
                PowerAction::PowerOff => (tr("Power off this computer?"), tr("Power Off")),
            };
            let cancel = button(canvas, text, MARGIN, y, tr("Cancel"), false, hovered(Hit::PowerCancel), focus_power(0));
            hits.push((cancel, Hit::PowerCancel));
            let confirm = button(canvas, text, cancel.x + cancel.w + 12, y, verb, true, hovered(Hit::PowerConfirm), focus_power(1));
            hits.push((confirm, Hit::PowerConfirm));
            if osk_at.is_some() {
                // The keyboard is just above: the question goes beside the buttons.
                let x = confirm.x + confirm.w + 16;
                label(canvas, text, question, x as f32, baseline(text, y, g(BUTTON_H), 15.0), 15.0, fg(), (w / 2 - x).max(0) as f32);
            } else {
                label(canvas, text, question, MARGIN as f32, (y - g(14)) as f32, 15.0, fg(), (w / 2) as f32);
            }
        }
    }

    // Session, bottom right; the accessibility button left of it.
    let bh = g(BUTTON_H);
    let mut right = w - MARGIN;
    let mut session_button = None;
    if let Some(name) = view.sessions.get(view.session) {
        let bw = width(text, name, 15.0).ceil() as i32 + 32 + 22;
        let r = Rect { x: w - MARGIN - bw, y, w: bw, h: bh };
        let open = view.session_menu.is_some();
        theme::control(canvas, r.x, r.y, r.w, r.h, Look { hovered: hovered(Hit::SessionButton) || open, ..Default::default() });
        if view.focus == Focus::Session {
            control_ring(canvas, r);
        }
        label(canvas, text, name, (r.x + 16) as f32, baseline(text, r.y, r.h, 15.0), 15.0, fg(), (bw - 48) as f32);
        // A chevron: up while the menu (above) is open.
        let towards = if open { theme::Towards::Up } else { theme::Towards::Down };
        theme::chevron(canvas, f64::from(r.x + r.w - 22), f64::from(r.y + r.h / 2), 9.0, towards, fg());
        hits.push((r, Hit::SessionButton));
        right = r.x - 12;
        session_button = Some(r);
    }
    let a11y = Rect { x: right - bh, y, w: bh, h: bh };
    let a11y_open = view.a11y_menu.is_some();
    theme::button(canvas, None, a11y.tuple(), "", Kind::Normal, Look { hovered: hovered(Hit::A11yButton) || a11y_open, ..Default::default() }, view.focus == Focus::A11y);
    a11y_symbol(canvas, f64::from(a11y.x) + f64::from(a11y.w) / 2.0, f64::from(a11y.y) + f64::from(a11y.h) / 2.0, theme::button_text(Kind::Normal));
    hits.push((a11y, Hit::A11yButton));

    let item_h = g(MENU_ITEM_H);
    if let (Some(highlighted), Some(button)) = (view.session_menu, session_button) {
        let widest = view.sessions.iter().map(|s| width(text, s, 15.0).ceil() as i32).max().unwrap_or(0);
        let mw = (widest + 36 + 24).max(button.w).min(w - 2 * MARGIN);
        for (i, (item, s)) in menu(canvas, w - MARGIN, y, mw, view.sessions.len(), item_h).into_iter().zip(&view.sessions).enumerate() {
            theme::row(canvas, item.x, item.y, item.w, item.h, Look { hovered: i == highlighted || hovered(Hit::SessionItem(i)), ..Default::default() });
            if i == view.session {
                // A check mark.
                let (cx, cy) = (f64::from(item.x + 14), f64::from(item.y + item.h / 2));
                let mark = if hc { WHITE } else { accent() };
                canvas.line((cx - 5.0, cy), (cx - 1.5, cy + 4.0), 2.0, mark);
                canvas.line((cx - 1.5, cy + 4.0), (cx + 5.0, cy - 4.5), 2.0, mark);
            }
            label(canvas, text, s, (item.x + 30) as f32, baseline(text, item.y, item.h, 15.0), 15.0, fg(), (item.w - 36) as f32);
            hits.push((item, Hit::SessionItem(i)));
        }
    }
    if let Some(highlighted) = view.a11y_menu {
        let widest = Setting::ALL.iter().map(|s| width(text, s.name(), 15.0).ceil() as i32).max().unwrap_or(0);
        let mw = (widest + 12 + 24 + theme::SWITCH_W as i32 + 12 + 2 * theme::POPOVER_PAD).max(g(240)).min(w - 2 * MARGIN);
        let right = (a11y.x + a11y.w).max(mw + MARGIN);
        for (i, (item, setting)) in menu(canvas, right, y, mw, Setting::ALL.len(), item_h.max(theme::SWITCH_H as i32 + 12)).into_iter().zip(Setting::ALL).enumerate() {
            let look = Look { hovered: i == highlighted || hovered(Hit::A11yItem(setting)), ..Default::default() };
            theme::row(canvas, item.x, item.y, item.w, item.h, look);
            let room = item.w - 12 - 24 - theme::SWITCH_W as i32 - 12;
            label(canvas, text, setting.name(), (item.x + 12) as f32, baseline(text, item.y, item.h, 15.0), 15.0, fg(), room as f32);
            let sx = f64::from(item.x + item.w - 12) - theme::SWITCH_W;
            let sy = f64::from(item.y) + (f64::from(item.h) - theme::SWITCH_H) / 2.0;
            theme::switch(canvas, sx, sy, setting.get(&view.accessibility), Look::default());
            hits.push((item, Hit::A11yItem(setting)));
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::View;
    use crate::login::{Note, Question};
    use crate::power::Offered;
    use crate::state::Accessibility;

    const W: i32 = 1280;
    const H: i32 = 800;

    /// High contrast, the text scale and the accent are the process's:
    /// painting tests take turns.
    static PAINTING: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        PAINTING.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn view(main: Main) -> View {
        View {
            time: "14:05".into(),
            date: "Thursday, November 12".into(),
            banner: Some("Authorized use only.".into()),
            main,
            notes: Vec::new(),
            sessions: vec!["Helix".into(), "Ubuntu".into(), "Xfce Session (Wayland)".into()],
            session: 0,
            session_menu: None,
            power: PowerView::Buttons(Offered { restart: true, power_off: true }),
            focus: Focus::Main,
            hover: None,
            accessibility: Accessibility::default(),
            a11y_menu: None,
            osk: None,
        }
    }

    fn users() -> Main {
        Main::Users { names: vec!["Ann Smith".into(), "bob".into(), "Carol".into()], logins: vec!["ann".into(), "bob".into(), "carol".into()], selected: 1 }
    }

    fn password(text: &str, busy: bool) -> Main {
        Main::Login { user: "Ann Smith".into(), login: "ann".into(), question: Some(Question { prompt: "Password:".into(), secret: true }), text: text.into(), busy }
    }

    struct Painted {
        data: Vec<u8>,
        hits: Hits,
        scale: f64,
    }

    impl Painted {
        /// The pixel at logical (x, y), as 0xAARRGGBB.
        fn px(&self, x: i32, y: i32) -> u32 {
            let width = helix_draw::physical(W as usize, self.scale);
            let (px, py) = ((f64::from(x) * self.scale) as usize, (f64::from(y) * self.scale) as usize);
            let i = (py * width + px) * 4;
            let [b, g, r, a] = [self.data[i], self.data[i + 1], self.data[i + 2], self.data[i + 3]];
            u32::from_be_bytes([a, r, g, b])
        }

        fn rect(&self, hit: Hit) -> Rect {
            self.hits.iter().find(|(_, h)| *h == hit).unwrap_or_else(|| panic!("no {hit:?} in {:?}", self.hits)).0
        }

        fn rects(&self, f: impl Fn(&Hit) -> bool) -> Vec<Rect> {
            self.hits.iter().filter(|(_, h)| f(h)).map(|(r, _)| *r).collect()
        }
    }

    /// Paint `view` with its accessibility settings in effect (high
    /// contrast, the text scale), as the greeter applies them.
    fn render(view: &View, scale: f64, name: &str) -> Painted {
        let _turn = lock();
        helix_draw::set_high_contrast(view.accessibility.high_contrast);
        theme::set_text_scale(view.accessibility.text_scale());
        let text = Text::load();
        let (pw, ph) = (helix_draw::physical(W as usize, scale), helix_draw::physical(H as usize, scale));
        let mut data = vec![0u8; pw * ph * 4];
        let mut canvas = Canvas::scaled(&mut data, W as usize, H as usize, scale).mirrored(helix_draw::rtl());
        let hits = paint(&mut canvas, text.as_ref(), view, W, H);
        helix_draw::set_high_contrast(false);
        theme::set_text_scale(1.0);
        if let Some(dir) = std::env::var_os("HELIX_GREETER_PNG_DIR") {
            write_png(&std::path::Path::new(&dir).join(format!("{name}.png")), &data, pw, ph);
        }
        Painted { data, hits, scale }
    }

    fn write_png(path: &std::path::Path, bgra: &[u8], w: usize, h: usize) {
        let rgba: Vec<u8> = bgra.chunks(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
        let file = std::fs::File::create(path).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().unwrap().write_image_data(&rgba).unwrap();
    }

    #[test]
    fn background_is_a_vertical_gradient() {
        let _turn = lock();
        let mut data = vec![0u8; 4 * 100 * 4];
        let mut canvas = Canvas::scaled(&mut data, 4, 100, 1.0);
        paint_background(&mut canvas);
        let row = |y: usize| &data[y * 16..y * 16 + 4];
        assert_eq!(row(0), &[0x3B, 0x2A, 0x26, 255]);
        assert_eq!(row(99), &[0x15, 0x10, 0x0F, 255]);
        assert_eq!(&data[0..4], &data[12..16], "flat across a row");
        assert!(row(50)[0] < row(0)[0] && row(50)[0] > row(99)[0]);
    }

    #[test]
    fn user_list() {
        let p = render(&view(users()), 1.0, "users");
        let rows: Vec<Rect> = (0..3).map(|i| p.rect(Hit::User(i))).collect();
        assert!(rows.windows(2).all(|r| r[1].y > r[0].y), "a column");
        assert_eq!(rows[0].x, (W - 360) / 2, "centered");
        // The selected row has the focus ring and the selected fill.
        let sel = rows[1];
        assert_eq!(p.px(sel.x - 1, sel.y + 30), accent());
        assert_eq!(p.px(sel.x + sel.w - 4, sel.y + 4), ROW_SELECTED);
        // Others don't.
        assert_ne!(p.px(rows[0].x - 1, rows[0].y + 30), accent());
        // Power at the bottom left, the session at the bottom right, the
        // accessibility button left of it.
        let restart = p.rect(Hit::Power(PowerAction::Restart));
        let session = p.rect(Hit::SessionButton);
        let a11y = p.rect(Hit::A11yButton);
        assert_eq!((restart.x, restart.y + restart.h), (MARGIN, H - MARGIN));
        assert_eq!(session.x + session.w, W - MARGIN);
        assert_eq!((a11y.x + a11y.w + 12, a11y.y, a11y.h), (session.x, session.y, session.h));
        assert_eq!(hit_at(&p.hits, f64::from(sel.x + 5), f64::from(sel.y + 5)), Some(Hit::User(1)));
        assert_eq!(hit_at(&p.hits, 5.0, 5.0), None);
        // Clock text is drawn (light pixels near the top center), if a font exists.
        if Text::load().is_some() {
            let base = (H / 8).max(72) + 20;
            let lit = (W / 2 - 80..W / 2 + 80).any(|x| (base - 40..base).any(|y| p.px(x, y) & 0xFF > 0xC0));
            assert!(lit, "the clock");
        }
    }

    /// A user with a picture shows it in the round avatar (green here),
    /// the others their initials.
    #[test]
    fn pictures_in_the_list() {
        let _turn = lock();
        let text = Text::load();
        let v = view(Main::Users { names: vec!["Ann Smith".into(), "bob".into()], logins: vec!["ann".into(), "bob".into()], selected: 0 });
        let green = helix_draw::Image { width: 8, height: 8, rgba: [0, 200, 0, 255].repeat(64) };
        let pictures: Pictures = [("ann".to_owned(), green)].into();
        let mut data = vec![0u8; (W * H * 4) as usize];
        let hits = {
            let mut canvas = Canvas::scaled(&mut data, W as usize, H as usize, 1.0);
            paint_with(&mut canvas, text.as_ref(), &v, W, H, &pictures)
        };
        if let Some(dir) = std::env::var_os("HELIX_GREETER_PNG_DIR") {
            write_png(&std::path::Path::new(&dir).join("pictures.png"), &data, W as usize, H as usize);
        }
        let p = Painted { data, hits, scale: 1.0 };
        let ann = p.rect(Hit::User(0));
        let bob = p.rect(Hit::User(1));
        let center = |r: Rect| p.px(r.x + 12 + 20, r.y + ROW_H / 2);
        let ann_px = center(ann);
        assert!(((ann_px >> 8) & 0xFF) > 150 && ((ann_px >> 16) & 0xFF) < 60, "Ann's picture: {ann_px:08x}");
        let bob_px = center(bob);
        assert!(!(((bob_px >> 8) & 0xFF) > 150 && ((bob_px >> 16) & 0xFF) < 60), "Bob's initials: {bob_px:08x}");
    }

    #[test]
    fn many_users_scroll_to_the_selection() {
        let names: Vec<String> = (0..30).map(|i| format!("User {i}")).collect();
        let logins = names.clone();
        let p = render(&view(Main::Users { names, logins, selected: 25 }), 1.0, "many-users");
        assert!(p.hits.iter().any(|(_, h)| *h == Hit::User(25)));
        assert!(!p.hits.iter().any(|(_, h)| *h == Hit::User(0)));
        let last = p.rect(Hit::User(25));
        assert!(last.y + last.h < H - BUTTON_H - MARGIN, "clear of the buttons");
    }

    #[test]
    fn password_prompt() {
        let p = render(&view(password("secret", false)), 1.0, "password");
        let field = p.rect(Hit::Field);
        assert_eq!(p.px(field.x - 1, field.y + 20), accent(), "focused ring");
        // Six dots, the first at x+20.
        assert_eq!(p.px(field.x + 20, field.y + field.h / 2), FG);
        assert_eq!(p.px(field.x + 20 + 5 * 14, field.y + field.h / 2), FG);
        assert_eq!(p.px(field.x + 20 + 7 * 14, field.y + field.h / 2), FIELD_BG);
        // The caret after the last dot.
        assert_eq!(p.px(field.x + 14 + 6 * 14, field.y + 12), FG);
        p.rect(Hit::Back);

        // Checking: no ring, no caret, the dots stay.
        let p = render(&view(password("secret", true)), 1.0, "password-checking");
        let field = p.rect(Hit::Field);
        assert_ne!(p.px(field.x - 1, field.y + 20), accent());
        assert_eq!(p.px(field.x + 20, field.y + field.h / 2), FG);
        assert_eq!(p.px(field.x + 14 + 6 * 14, field.y + 12), FIELD_BG);
    }

    #[test]
    fn error_message() {
        let mut v = view(password("", false));
        v.notes = vec![Note { text: crate::login::RETRY_MESSAGE.into(), error: true }];
        let p = render(&v, 1.0, "error");
        let field = p.rect(Hit::Field);
        let back = p.rect(Hit::Back);
        assert!(back.y > field.y + field.h + 20, "the message between field and Back");
        if Text::load().is_some() {
            let red = (field.x..field.x + 300).any(|x| (field.y + field.h..back.y).any(|y| {
                let c = p.px(x, y);
                (c >> 16 & 0xFF) > 0xC0 && (c & 0xFF) < 0xA0
            }));
            assert!(red, "the error is drawn in the error color");
        }
        // Visible questions show their text; the username field.
        let mut v = view(Main::Login { user: "ann".into(), login: "ann".into(), question: Some(Question { prompt: "Token:".into(), secret: false }), text: "123456".into(), busy: false });
        v.notes = vec![Note { text: "Touch your security key".into(), error: false }];
        render(&v, 1.0, "visible-question");
        render(&view(Main::Username { text: String::new() }), 1.0, "username");
        render(&view(Main::Login { user: "ann".into(), login: "ann".into(), question: None, text: String::new(), busy: true }), 1.0, "waiting");
    }

    #[test]
    fn session_menu() {
        let mut v = view(users());
        v.session_menu = Some(1);
        v.focus = Focus::Session;
        let p = render(&v, 1.0, "session-menu");
        let button = p.rect(Hit::SessionButton);
        let items: Vec<Rect> = (0..3).map(|i| p.rect(Hit::SessionItem(i))).collect();
        assert!(items[2].y + items[2].h < button.y, "the menu opens above the button");
        assert_eq!(p.px(button.x - 3, button.y + 10), accent(), "focused");
        assert_eq!(p.px(items[1].x + items[1].w - 8, items[1].y + 18), theme::CONTROL_HOVER, "highlighted item");
        // Items are on top of anything under them.
        assert_eq!(hit_at(&p.hits, f64::from(items[0].x + 4), f64::from(items[0].y + 4)), Some(Hit::SessionItem(0)));
    }

    #[test]
    fn power_confirmation_and_scale() {
        let mut v = view(users());
        v.power = PowerView::Confirm(PowerAction::PowerOff);
        v.focus = Focus::Power(1);
        let p = render(&v, 1.5, "power-confirm-1.5x");
        let confirm = p.rect(Hit::PowerConfirm);
        let cancel = p.rect(Hit::PowerCancel);
        assert!(confirm.x > cancel.x);
        assert_eq!(p.px(confirm.x + 4, confirm.y + 4), helix_draw::accent::current().fill);
        // Tiny monitors don't panic, with the keyboard and large text too.
        let _turn = lock();
        let mut data = vec![0u8; 200 * 150 * 4];
        let mut canvas = Canvas::scaled(&mut data, 200, 150, 1.0);
        paint(&mut canvas, Text::load().as_ref(), &view(password("x", false)), 200, 150);
        theme::set_text_scale(1.25);
        let mut v = view(password("x", false));
        v.osk = Some(Osk::default());
        v.a11y_menu = Some(0);
        paint(&mut canvas, Text::load().as_ref(), &v, 200, 150);
        theme::set_text_scale(1.0);
    }

    /// The accessibility menu: above its button, a switch per setting,
    /// on where the setting is.
    #[test]
    fn accessibility_menu() {
        let mut v = view(users());
        v.a11y_menu = Some(2);
        v.focus = Focus::A11y;
        v.accessibility.screen_reader = true;
        let p = render(&v, 1.0, "a11y-menu");
        let button = p.rect(Hit::A11yButton);
        let items: Vec<Rect> = Setting::ALL.iter().map(|s| p.rect(Hit::A11yItem(*s))).collect();
        assert!(items.windows(2).all(|r| r[1].y == r[0].y + r[0].h));
        assert!(items[3].y + items[3].h < button.y, "above the button");
        assert!(items[0].x >= MARGIN && items[0].x + items[0].w <= W - MARGIN);
        // The switches: on (the accent's fill) for the screen reader only.
        // The switch's left end: the accent's fill when on (the knob is
        // at the right), the knob when off.
        let left_end = |r: Rect| p.px(r.x + r.w - 12 - theme::SWITCH_W as i32 + 6, r.y + r.h / 2);
        assert_eq!(left_end(items[2]), helix_draw::accent::current().fill, "on");
        assert_ne!(left_end(items[0]), helix_draw::accent::current().fill, "off");
        assert_eq!(hit_at(&p.hits, f64::from(items[1].x + 3), f64::from(items[1].y + 3)), Some(Hit::A11yItem(Setting::LargeText)));
    }

    /// High contrast: a black background, white text, white outlines;
    /// the focus ring stays visible.
    #[test]
    fn high_contrast() {
        let mut v = view(users());
        v.accessibility.high_contrast = true;
        v.hover = Some(Hit::User(0));
        let p = render(&v, 1.0, "hc-users");
        assert_eq!(p.px(5, 5), BLACK);
        assert_eq!(p.px(W / 2, H - 5), BLACK, "no gradient");
        let rows: Vec<Rect> = (0..3).map(|i| p.rect(Hit::User(i))).collect();
        let sel = rows[1];
        assert_eq!(p.px(sel.x + 1, sel.y + 30), WHITE, "the selected row is outlined");
        assert_eq!(p.px(sel.x + 40, sel.y + 4), BLACK, "not filled");
        assert_eq!(p.px(sel.x - 2, sel.y + 30), BLACK, "a gap");
        assert_eq!(p.px(sel.x - 4, sel.y + 30), WHITE, "the focus ring outside it");
        assert_eq!(p.px(rows[0].x, rows[0].y + 30), WHITE, "the hovered row: a thin outline");
        assert_eq!(p.px(rows[2].x, rows[2].y + 30), BLACK, "others: none");
        let restart = p.rect(Hit::Power(PowerAction::Restart));
        assert!((restart.y..restart.y + 3).any(|y| p.px(restart.x + restart.w / 2, y) == WHITE), "buttons are outlined");
        assert_eq!(p.px(restart.x + 4, restart.y + restart.h / 2), BLACK);
        if Text::load().is_some() {
            let base = (H / 8).max(72) + 20;
            let white = (W / 2 - 80..W / 2 + 80).any(|x| (base - 40..base).any(|y| p.px(x, y) == WHITE));
            assert!(white, "white text");
        }

        let mut v = view(password("abc", false));
        v.accessibility.high_contrast = true;
        let p = render(&v, 1.0, "hc-password");
        let field = p.rect(Hit::Field);
        assert_eq!(p.px(field.x - 1, field.y + 20), WHITE, "the focused field's ring");
        assert_eq!(p.px(field.x + 4, field.y + 4), BLACK);
        assert_eq!(p.px(field.x + 20, field.y + field.h / 2), WHITE, "white dots");
        let mut v = view(password("abc", true));
        v.accessibility.high_contrast = true;
        let p = render(&v, 1.0, "hc-password-checking");
        let field = p.rect(Hit::Field);
        assert_eq!(p.px(field.x - 1, field.y + 20), WHITE, "unfocused fields are outlined too");
        assert_eq!(p.px(field.x - 2, field.y + 20), BLACK, "thinly");

        let mut v = view(users());
        v.accessibility.high_contrast = true;
        v.accessibility.large_text = true;
        v.a11y_menu = Some(0);
        v.accessibility.screen_keyboard = true;
        v.osk = Some(Osk::default());
        v.focus = Focus::A11y;
        render(&v, 1.0, "hc-large-a11y-menu");
        let mut v = view(users());
        v.accessibility.high_contrast = true;
        v.session_menu = Some(1);
        v.focus = Focus::Session;
        render(&v, 1.0, "hc-session-menu");
    }

    /// Large text: rows, fields and buttons grow with the text, and
    /// nothing runs into anything else.
    #[test]
    fn large_text() {
        let mut v = view(password("secret", false));
        v.accessibility.large_text = true;
        v.notes = vec![Note { text: crate::login::RETRY_MESSAGE.into(), error: true }];
        let p = render(&v, 1.0, "large-password");
        let field = p.rect(Hit::Field);
        let back = p.rect(Hit::Back);
        assert_eq!(field.h, 55, "44 × 1.25");
        assert_eq!(back.h, 45, "36 × 1.25");
        assert!(back.y > field.y + field.h + 20);
        let restart = p.rect(Hit::Power(PowerAction::Restart));
        assert_eq!((restart.h, restart.y + restart.h), (45, H - MARGIN));
        assert!(back.y + back.h < restart.y);
        let mut v = view(users());
        v.accessibility.large_text = true;
        let p = render(&v, 1.0, "large-users");
        let rows: Vec<Rect> = (0..3).map(|i| p.rect(Hit::User(i))).collect();
        assert_eq!(rows[0].h, 70, "56 × 1.25");
        let session = p.rect(Hit::SessionButton);
        let a11y = p.rect(Hit::A11yButton);
        assert!(!a11y.overlaps(&session) && a11y.h == session.h);
        assert!(rows[2].y + rows[2].h < session.y);
        if Text::load().is_some() {
            let normal = render(&view(users()), 1.0, "users");
            assert!(session.w > normal.rect(Hit::SessionButton).w, "wider for the larger name");
        }
    }

    /// The on-screen keyboard: every key placed and clickable, between
    /// the column and the bottom bar, in each layout.
    #[test]
    fn screen_keyboard() {
        for (name, large, hc, main) in [
            ("osk-users", false, false, users()),
            ("osk-password", false, false, password("secret", false)),
            ("osk-large-password", true, false, password("secret", false)),
            ("osk-hc-large-username", true, true, Main::Username { text: "ann".into() }),
        ] {
            let mut v = view(main);
            v.accessibility = Accessibility { high_contrast: hc, large_text: large, screen_keyboard: true, ..Default::default() };
            v.notes = vec![Note { text: crate::login::RETRY_MESSAGE.into(), error: true }];
            let osk = Osk { page: crate::osk::Page::Letters, shift: name == "osk-password" };
            v.osk = Some(osk);
            v.hover = Some(Hit::OskKey(3));
            let p = render(&v, 1.0, name);
            let keys = p.rects(|h| matches!(h, Hit::OskKey(_)));
            assert_eq!(keys.len(), osk.keys().len(), "{name}");
            let bar = p.rect(Hit::A11yButton).y.min(p.rect(Hit::Power(PowerAction::Restart)).y);
            let column = p.rects(|h| matches!(h, Hit::User(_) | Hit::Field | Hit::Back));
            assert!(!column.is_empty());
            for k in &keys {
                assert!(k.y + k.h < bar - 12, "{name}: {k:?} clear of the bottom bar");
                assert!(k.x >= MARGIN && k.x + k.w <= W - MARGIN, "{name}: {k:?}");
                for c in &column {
                    assert!(!k.overlaps(c), "{name}: key {k:?} on {c:?}");
                }
                let hit = hit_at(&p.hits, f64::from(k.x + k.w / 2), f64::from(k.y + k.h / 2));
                assert!(matches!(hit, Some(Hit::OskKey(_))), "{name}");
            }
            // Keys of a row do not overlap.
            for (i, a) in keys.iter().enumerate() {
                for b in &keys[i + 1..] {
                    assert!(!a.overlaps(b), "{name}: {a:?} {b:?}");
                }
            }
        }
        let mut v = view(users());
        v.accessibility.screen_keyboard = true;
        v.osk = Some(Osk { page: crate::osk::Page::Symbols, shift: false });
        render(&v, 1.0, "osk-symbols");
        // Confirming a power action: the question beside the buttons, not
        // under the keyboard.
        v.power = PowerView::Confirm(PowerAction::Restart);
        v.focus = Focus::Power(1);
        v.osk = Some(Osk { page: crate::osk::Page::Symbols, shift: true });
        let p = render(&v, 1.0, "osk-power-confirm");
        let confirm = p.rect(Hit::PowerConfirm);
        if Text::load().is_some() {
            let lit = (confirm.x + confirm.w + 16..confirm.x + confirm.w + 120).any(|x| (confirm.y..confirm.y + confirm.h).any(|y| p.px(x, y) & 0xFF > 0xC0));
            assert!(lit, "the question beside the buttons");
        }
    }

    /// The greeter in several accents (`accent` in its configuration):
    /// `HELIX_GREETER_PNG_DIR=dir cargo test -p helix-greeter accents --
    /// --ignored --test-threads=1`. It changes the process's accent, so it
    /// runs alone.
    #[test]
    #[ignore]
    fn accents() {
        use helix_draw::accent::Accent;
        for accent in [Accent::Blue, Accent::Orange, Accent::Yellow, Accent::Slate] {
            helix_draw::accent::set(accent);
            let name = accent.name();
            let p = render(&view(password("secret", false)), 1.0, &format!("accent-{name}-password"));
            let field = p.rect(Hit::Field);
            assert_eq!(p.px(field.x - 1, field.y + 20), accent.palette().color);
            let mut v = view(users());
            v.power = PowerView::Confirm(PowerAction::PowerOff);
            v.focus = Focus::Power(0);
            let p = render(&v, 1.0, &format!("accent-{name}-power"));
            let confirm = p.rect(Hit::PowerConfirm);
            assert_eq!(p.px(confirm.x + 4, confirm.y + 4), accent.palette().fill);
            let mut v = view(users());
            v.session_menu = Some(1);
            v.focus = Focus::Session;
            render(&v, 1.0, &format!("accent-{name}-sessions"));
        }
        helix_draw::accent::set(Accent::Blue);
    }

    /// The greeter in the environment's locale (helixdesktop/spec
    /// `components/i18n.md`): `HELIX_LOCALEDIR`, `LANGUAGE`, `LANG` (and
    /// `LOCPATH`) pick the catalogue, the clock's names and the sessions'
    /// `Name[xx]`; a right-to-left translation mirrors the screen.
    /// `HELIX_GREETER_PNG_DIR=dir HELIX_GALLERY_TAG=he cargo test -p
    /// helix-greeter locale -- --ignored` writes `locale-he-…`.
    #[test]
    #[ignore]
    fn locale() {
        helix_i18n::init_for_tests();
        helix_draw::set_rtl(helix_i18n::is_rtl());
        let tag = std::env::var("HELIX_GALLERY_TAG").unwrap_or_else(|_| "locale".into());
        crate::clock::decide(None, None);
        // Saturday 14 November 2026, 14:05 in New York (any time will do).
        let now = 1_794_683_100;
        let entry = "[Desktop Entry]\nName=Helix\nName[de]=Helix (Deutsch)\nName[he]=\u{5d4}\u{5dc}\u{5d9}\u{5e7}\u{5e1}\nExec=helix-session\n";
        let helix = crate::sessions::parse("helix", entry, |_| true).unwrap().name;
        let base = |main| View { time: crate::clock::time(now), date: crate::clock::date(now), sessions: vec![helix.clone(), "Ubuntu".into()], ..view(main) };
        render(&base(users()), 1.0, &format!("locale-{tag}-users"));
        let mut v = base(password("secret", false));
        v.notes = vec![Note { text: helix_i18n::tr(crate::login::RETRY_MESSAGE).into(), error: true }];
        render(&v, 1.0, &format!("locale-{tag}-password-retry"));
        let mut v = base(users());
        v.power = PowerView::Confirm(PowerAction::Restart);
        v.focus = Focus::Power(0);
        render(&v, 1.0, &format!("locale-{tag}-restart"));
        let mut v = base(users());
        v.session_menu = Some(0);
        v.focus = Focus::Session;
        render(&v, 1.0, &format!("locale-{tag}-sessions"));
        let mut v = base(users());
        v.a11y_menu = Some(0);
        v.focus = Focus::A11y;
        render(&v, 1.0, &format!("locale-{tag}-accessibility"));
        render(&base(Main::Username { text: String::new() }), 1.0, &format!("locale-{tag}-username"));
        helix_draw::set_rtl(false);
    }
}
