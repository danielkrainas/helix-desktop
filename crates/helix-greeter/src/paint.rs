//! Painting the greeter: a pure function of the [`View`] into a
//! [`Canvas`] (helix-desktop-spec `components/greeter.md`, "What the
//! screen shows"). The look follows the lock screen's: a centered column,
//! text fields as in "Text fields" (`components/README.md`).

use helix_draw::{Canvas, Style, Text};

use crate::app::{Focus, Hit, Main, PowerView, View};
use crate::power::PowerAction;

const FG: u32 = 0xFFEDEDEF;
const DIM_FG: u32 = 0xFF9A9AA4;
const ERROR_FG: u32 = 0xFFFF8A80;
const ACCENT: u32 = 0xFF3584E4;
const ACCENT_HOVER: u32 = 0xFF4A92EA;
const FIELD_BG: u32 = 0xFF2C2C33;
const BUTTON: u32 = 0xFF33343D;
const BUTTON_HOVER: u32 = 0xFF41424D;
const ROW_SELECTED: u32 = 0xFF30313B;
const ROW_HOVER: u32 = 0xFF292A33;
const MENU_BG: u32 = 0xFF24252D;
const MENU_BORDER: u32 = 0xFF3C3D48;
/// The background: a vertical gradient.
const BG_TOP: u32 = 0xFF262A3B;
const BG_BOTTOM: u32 = 0xFF0F1015;

const ROW_H: i32 = 56;
const ROW_GAP: i32 = 6;
const FIELD_H: i32 = 44;
const BUTTON_H: i32 = 36;
const MARGIN: i32 = 24;
const MENU_ITEM_H: i32 = 36;

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
}

/// Where clicks land: later entries are on top.
pub type Hits = Vec<(Rect, Hit)>;

/// The topmost target under (x, y).
pub fn hit_at(hits: &Hits, x: f64, y: f64) -> Option<Hit> {
    hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
}

fn lerp(a: u32, b: u32, t: f32) -> [u8; 3] {
    let c = |c: u32, shift: u32| ((c >> shift) & 0xFF) as f32;
    [16, 8, 0].map(|s| (c(a, s) + (c(b, s) - c(a, s)) * t).round() as u8)
}

/// The dark vertical gradient every monitor shows.
pub fn paint_background(canvas: &mut Canvas) {
    let (width, height) = (canvas.width, canvas.height);
    for y in 0..height {
        let t = if height > 1 { y as f32 / (height - 1) as f32 } else { 0.0 };
        let [r, g, b] = lerp(BG_TOP, BG_BOTTOM, t);
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

fn width(text: Option<&Text>, s: &str, px: f32) -> f32 {
    text.map_or(s.chars().count() as f32 * px * 0.55, |t| t.width(s, px))
}

/// Baseline that vertically centers `px` text in a box.
fn baseline(text: Option<&Text>, top: i32, height: i32, px: f32) -> f32 {
    let cap = text.map_or(px * 0.7, |t| t.ascent(px) * 0.72);
    top as f32 + (height as f32 + cap) / 2.0
}

fn centered(canvas: &mut Canvas, text: Option<&Text>, s: &str, w: i32, base: f32, px: f32, color: u32) {
    let max_w = (w - 2 * MARGIN) as f32;
    let x = ((w as f32 - width(text, s, px).min(max_w)) / 2.0).max(MARGIN as f32);
    label(canvas, text, s, x, base, px, color, max_w);
}

/// A 2-unit accent ring just outside `r`: keyboard focus.
fn focus_ring(canvas: &mut Canvas, r: Rect, color: u32) {
    let o = r.grow(2);
    canvas.rect(o.x, o.y, o.w, 2, color);
    canvas.rect(o.x, o.y + o.h - 2, o.w, 2, color);
    canvas.rect(o.x, o.y, 2, o.h, color);
    canvas.rect(o.x + o.w - 2, o.y, 2, o.h, color);
}

/// A text field's box: an accent ring while it takes typing, a faint
/// outline otherwise.
fn text_field(canvas: &mut Canvas, r: Rect, focused: bool) {
    let (ring, width) = if focused { (ACCENT, 2) } else { (0x30FFFFFF, 1) };
    let o = r.grow(width);
    canvas.rect(o.x, o.y, o.w, o.h, ring);
    canvas.rect(r.x, r.y, r.w, r.h, FIELD_BG);
}

/// The caret at `x`, the height of a line of `px` text in field `r`.
fn caret(canvas: &mut Canvas, r: Rect, x: i32, px: f32) {
    let h = (px * 1.3).round() as i32;
    canvas.rect(x, r.y + (r.h - h) / 2, 2, h, FG);
}

/// The contents of a field: dots for hidden text, else the text (its end
/// when too long), a placeholder when empty; the caret when focused.
fn field_contents(canvas: &mut Canvas, text: Option<&Text>, field: Rect, value: &str, secret: bool, placeholder: &str, focused: bool) {
    const PX: f32 = 17.0;
    let inner = field.w - 34;
    let end = if value.is_empty() {
        label(canvas, text, placeholder, (field.x + 20) as f32, baseline(text, field.y, field.h, PX), PX, DIM_FG, inner as f32);
        field.x + 14
    } else if secret {
        // One dot per character (drawn: not every font has a bullet).
        let count = value.chars().count().min((inner / 14).max(0) as usize) as i32;
        for i in 0..count {
            canvas.circle(f64::from(field.x + 20 + i * 14), f64::from(field.y + field.h / 2), 5.0, FG);
        }
        field.x + 14 + count * 14
    } else {
        // The end of a long answer stays in view.
        let mut shown: &str = value;
        while width(text, shown, PX) > inner as f32 {
            let mut chars = shown.chars();
            chars.next();
            shown = chars.as_str();
        }
        label(canvas, text, shown, (field.x + 16) as f32, baseline(text, field.y, field.h, PX), PX, FG, f32::MAX);
        field.x + 18 + width(text, shown, PX).ceil() as i32
    };
    if focused {
        caret(canvas, field, end, PX);
    }
}

/// A round picture standing in for the user: their initial on a color
/// picked from their name.
fn avatar(canvas: &mut Canvas, text: Option<&Text>, name: &str, cx: i32, cy: i32, r: i32) {
    // The lock screen's avatar: the same colors and initials.
    helix_draw::screen::avatar(canvas, text, None, name, None, f64::from(cx), f64::from(cy), f64::from(r), 1.0);
}

/// A push button; `strong` for the action of a question.
#[allow(clippy::too_many_arguments)]
fn button(canvas: &mut Canvas, text: Option<&Text>, x: i32, y: i32, s: &str, strong: bool, hovered: bool, focused: bool) -> Rect {
    let w = width(text, s, 15.0).ceil() as i32 + 32;
    let r = Rect { x, y, w, h: BUTTON_H };
    let bg = match (strong, hovered) {
        (true, false) => ACCENT,
        (true, true) => ACCENT_HOVER,
        (false, false) => BUTTON,
        (false, true) => BUTTON_HOVER,
    };
    if focused {
        focus_ring(canvas, r, if strong { FG } else { ACCENT });
    }
    canvas.rect(r.x, r.y, r.w, r.h, bg);
    label(canvas, text, s, (r.x + 16) as f32, baseline(text, r.y, r.h, 15.0), 15.0, FG, r.w as f32);
    r
}

/// Messages under the field, from baseline `y`; returns the y below them.
fn notes(canvas: &mut Canvas, text: Option<&Text>, view: &View, x: i32, y: i32, w: i32) -> i32 {
    let mut base = y;
    for note in &view.notes {
        let lines = text.map(|t| t.wrap(&note.text, 15.0, w as f32, 3)).unwrap_or_else(|| vec![note.text.clone()]);
        for line in lines {
            label(canvas, text, &line, x as f32, base as f32, 15.0, if note.error { ERROR_FG } else { DIM_FG }, w as f32);
            base += 21;
        }
    }
    base
}

/// Paint the primary monitor's greeter, `w`×`h` logical; returns where
/// clicks land.
pub fn paint(canvas: &mut Canvas, text: Option<&Text>, view: &View, w: i32, h: i32) -> Hits {
    paint_background(canvas);
    let mut hits = Hits::new();
    let hovered = |hit: Hit| view.hover == Some(hit);

    // Clock, date and banner.
    let clock_base = (h / 8).max(72) + 20;
    centered(canvas, text, &view.time, w, clock_base as f32, 64.0, FG);
    centered(canvas, text, &view.date, w, (clock_base + 36) as f32, 18.0, DIM_FG);
    let mut header_end = clock_base + 36;
    if let Some(banner) = &view.banner {
        let lines = text.map(|t| t.wrap(banner, 15.0, (w - 2 * MARGIN).min(640) as f32, 3)).unwrap_or_default();
        for (i, line) in lines.iter().enumerate() {
            centered(canvas, text, line, w, (clock_base + 70 + 21 * i as i32) as f32, 15.0, FG);
        }
        header_end = clock_base + 70 + 21 * lines.len() as i32;
    }

    // The column.
    let col_w = 360.min(w - 2 * MARGIN).max(0);
    let col_x = (w - col_w) / 2;
    let top = (h * 34 / 100).max(header_end + 40);
    let bottom_bar = h - BUTTON_H - MARGIN;
    let main_focused = view.focus == Focus::Main;
    let notes_y = match &view.main {
        Main::Loading => top + 20,
        Main::Users { names, selected } => {
            let room = (bottom_bar - 24 - top).max(ROW_H);
            let visible = ((room + ROW_GAP) / (ROW_H + ROW_GAP)).max(1) as usize;
            let first = (selected + 1).saturating_sub(visible);
            let mut y = top;
            for (i, name) in names.iter().enumerate().skip(first).take(visible) {
                let row = Rect { x: col_x, y, w: col_w, h: ROW_H };
                if i == *selected {
                    if main_focused {
                        focus_ring(canvas, row, ACCENT);
                    }
                    canvas.rect(row.x, row.y, row.w, row.h, ROW_SELECTED);
                } else if hovered(Hit::User(i)) {
                    canvas.rect(row.x, row.y, row.w, row.h, ROW_HOVER);
                }
                avatar(canvas, text, name, col_x + 12 + 20, y + ROW_H / 2, 20);
                label(canvas, text, name, (col_x + 64) as f32, baseline(text, y, ROW_H, 17.0), 17.0, FG, (col_w - 76) as f32);
                hits.push((row, Hit::User(i)));
                y += ROW_H + ROW_GAP;
            }
            // More users than fit: a hint at the cut-off edge.
            if first > 0 {
                canvas.rect(col_x + col_w / 2 - 12, top - 10, 24, 2, DIM_FG);
            }
            if first + visible < names.len() {
                canvas.rect(col_x + col_w / 2 - 12, y + 2, 24, 2, DIM_FG);
            }
            y + 24
        }
        Main::Username { text: typed } => {
            let field = Rect { x: col_x, y: top + 20, w: col_w, h: FIELD_H };
            text_field(canvas, field, main_focused);
            field_contents(canvas, text, field, typed, false, "Username", main_focused);
            hits.push((field, Hit::Field));
            field.y + field.h + 30
        }
        Main::Login { user, question, text: typed, busy } => {
            avatar(canvas, text, user, w / 2, top + 36, 36);
            centered(canvas, text, user, w, (top + 104) as f32, 20.0, FG);
            let field = Rect { x: col_x, y: top + 150, w: col_w, h: FIELD_H };
            if let Some(q) = question {
                label(canvas, text, &q.prompt, col_x as f32, (field.y - 12) as f32, 15.0, DIM_FG, col_w as f32);
                // Waiting for greetd: no typing, so no ring or caret.
                let focused = main_focused && !busy;
                text_field(canvas, field, focused);
                field_contents(canvas, text, field, typed, q.secret, "", focused);
                hits.push((field, Hit::Field));
            }
            field.y + field.h + 30
        }
    };
    let mut below = notes(canvas, text, view, col_x, notes_y, col_w);
    if let Main::Login { .. } = view.main {
        below = below.max(notes_y) - 4;
        let back = button(canvas, text, col_x, below, "Back", false, hovered(Hit::Back), false);
        hits.push((back, Hit::Back));
    }

    // Power, bottom left.
    let y = h - BUTTON_H - MARGIN;
    let power_hits = view.power.hits();
    let focus_power = |n: usize| view.focus == Focus::Power(n);
    match view.power {
        PowerView::Buttons(_) => {
            let mut x = MARGIN;
            for (n, hit) in power_hits.iter().enumerate() {
                let s = if *hit == Hit::Power(PowerAction::Restart) { "Restart…" } else { "Power Off…" };
                let r = button(canvas, text, x, y, s, false, hovered(*hit), focus_power(n));
                hits.push((r, *hit));
                x = r.x + r.w + 12;
            }
        }
        PowerView::Confirm(action) => {
            let (question, verb) = match action {
                PowerAction::Restart => ("Restart this computer?", "Restart"),
                PowerAction::PowerOff => ("Power off this computer?", "Power Off"),
            };
            label(canvas, text, question, MARGIN as f32, (y - 14) as f32, 15.0, FG, (w / 2) as f32);
            let cancel = button(canvas, text, MARGIN, y, "Cancel", false, hovered(Hit::PowerCancel), focus_power(0));
            hits.push((cancel, Hit::PowerCancel));
            let confirm = button(canvas, text, cancel.x + cancel.w + 12, y, verb, true, hovered(Hit::PowerConfirm), focus_power(1));
            hits.push((confirm, Hit::PowerConfirm));
        }
    }

    // Session, bottom right.
    if let Some(name) = view.sessions.get(view.session) {
        let bw = width(text, name, 15.0).ceil() as i32 + 32 + 22;
        let r = Rect { x: w - MARGIN - bw, y, w: bw, h: BUTTON_H };
        if view.focus == Focus::Session {
            focus_ring(canvas, r, ACCENT);
        }
        let open = view.session_menu.is_some();
        canvas.rect(r.x, r.y, r.w, r.h, if hovered(Hit::SessionButton) || open { BUTTON_HOVER } else { BUTTON });
        label(canvas, text, name, (r.x + 16) as f32, baseline(text, r.y, r.h, 15.0), 15.0, FG, (bw - 48) as f32);
        // A chevron: up while the menu (above) is open.
        let (cx, cy) = (f64::from(r.x + r.w - 22), f64::from(r.y + r.h / 2));
        let d = if open { -3.0 } else { 3.0 };
        canvas.line((cx - 4.5, cy - d / 2.0), (cx, cy + d / 2.0 + 0.5 * d.signum()), 1.6, FG);
        canvas.line((cx, cy + d / 2.0 + 0.5 * d.signum()), (cx + 4.5, cy - d / 2.0), 1.6, FG);
        hits.push((r, Hit::SessionButton));

        if let Some(highlighted) = view.session_menu {
            let widest = view.sessions.iter().map(|s| width(text, s, 15.0).ceil() as i32).max().unwrap_or(0);
            let mw = (widest + 36 + 24).max(bw).min(w - 2 * MARGIN);
            let mh = view.sessions.len() as i32 * MENU_ITEM_H + 12;
            let menu = Rect { x: w - MARGIN - mw, y: (y - 8 - mh).max(MARGIN), w: mw, h: mh };
            canvas.round_rect(f64::from(menu.x - 1), f64::from(menu.y - 1), f64::from(menu.w + 2), f64::from(menu.h + 2), 9.0, MENU_BORDER);
            canvas.round_rect(f64::from(menu.x), f64::from(menu.y), f64::from(menu.w), f64::from(menu.h), 8.0, MENU_BG);
            for (i, s) in view.sessions.iter().enumerate() {
                let item = Rect { x: menu.x + 6, y: menu.y + 6 + i as i32 * MENU_ITEM_H, w: menu.w - 12, h: MENU_ITEM_H };
                if item.y + item.h > menu.y + menu.h {
                    break;
                }
                if i == highlighted || hovered(Hit::SessionItem(i)) {
                    canvas.round_rect(f64::from(item.x), f64::from(item.y), f64::from(item.w), f64::from(item.h), 5.0, BUTTON_HOVER);
                }
                if i == view.session {
                    // A check mark.
                    let (cx, cy) = (f64::from(item.x + 14), f64::from(item.y + item.h / 2));
                    canvas.line((cx - 5.0, cy), (cx - 1.5, cy + 4.0), 2.0, ACCENT);
                    canvas.line((cx - 1.5, cy + 4.0), (cx + 5.0, cy - 4.5), 2.0, ACCENT);
                }
                label(canvas, text, s, (item.x + 30) as f32, baseline(text, item.y, item.h, 15.0), 15.0, FG, (item.w - 36) as f32);
                hits.push((item, Hit::SessionItem(i)));
            }
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

    const W: i32 = 1280;
    const H: i32 = 800;

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
        }
    }

    fn users() -> Main {
        Main::Users { names: vec!["Ann Smith".into(), "bob".into(), "Carol".into()], selected: 1 }
    }

    fn password(text: &str, busy: bool) -> Main {
        Main::Login { user: "Ann Smith".into(), question: Some(Question { prompt: "Password:".into(), secret: true }), text: text.into(), busy }
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
    }

    fn render(view: &View, scale: f64, name: &str) -> Painted {
        let text = Text::load();
        let (pw, ph) = (helix_draw::physical(W as usize, scale), helix_draw::physical(H as usize, scale));
        let mut data = vec![0u8; pw * ph * 4];
        let mut canvas = Canvas::scaled(&mut data, W as usize, H as usize, scale);
        let hits = paint(&mut canvas, text.as_ref(), view, W, H);
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
        assert_eq!(p.px(sel.x - 1, sel.y + 30), ACCENT);
        assert_eq!(p.px(sel.x + sel.w - 4, sel.y + 4), ROW_SELECTED);
        // Others don't.
        assert_ne!(p.px(rows[0].x - 1, rows[0].y + 30), ACCENT);
        // Power at the bottom left, the session at the bottom right.
        let restart = p.rect(Hit::Power(PowerAction::Restart));
        let session = p.rect(Hit::SessionButton);
        assert_eq!((restart.x, restart.y + restart.h), (MARGIN, H - MARGIN));
        assert_eq!(session.x + session.w, W - MARGIN);
        assert_eq!(hit_at(&p.hits, f64::from(sel.x + 5), f64::from(sel.y + 5)), Some(Hit::User(1)));
        assert_eq!(hit_at(&p.hits, 5.0, 5.0), None);
        // Clock text is drawn (light pixels near the top center), if a font exists.
        if Text::load().is_some() {
            let base = (H / 8).max(72) + 20;
            let lit = (W / 2 - 80..W / 2 + 80).any(|x| (base - 40..base).any(|y| p.px(x, y) & 0xFF > 0xC0));
            assert!(lit, "the clock");
        }
    }

    #[test]
    fn many_users_scroll_to_the_selection() {
        let names: Vec<String> = (0..30).map(|i| format!("User {i}")).collect();
        let p = render(&view(Main::Users { names, selected: 25 }), 1.0, "many-users");
        assert!(p.hits.iter().any(|(_, h)| *h == Hit::User(25)));
        assert!(!p.hits.iter().any(|(_, h)| *h == Hit::User(0)));
        let last = p.rect(Hit::User(25));
        assert!(last.y + last.h < H - BUTTON_H - MARGIN, "clear of the buttons");
    }

    #[test]
    fn password_prompt() {
        let p = render(&view(password("secret", false)), 1.0, "password");
        let field = p.rect(Hit::Field);
        assert_eq!(p.px(field.x - 1, field.y + 20), ACCENT, "focused ring");
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
        assert_ne!(p.px(field.x - 1, field.y + 20), ACCENT);
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
        let mut v = view(Main::Login { user: "ann".into(), question: Some(Question { prompt: "Token:".into(), secret: false }), text: "123456".into(), busy: false });
        v.notes = vec![Note { text: "Touch your security key".into(), error: false }];
        render(&v, 1.0, "visible-question");
        render(&view(Main::Username { text: String::new() }), 1.0, "username");
        render(&view(Main::Login { user: "ann".into(), question: None, text: String::new(), busy: true }), 1.0, "waiting");
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
        assert_eq!(p.px(button.x - 1, button.y + 10), ACCENT, "focused");
        assert_eq!(p.px(items[1].x + items[1].w - 3, items[1].y + 18), BUTTON_HOVER, "highlighted item");
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
        assert_eq!(p.px(confirm.x + 4, confirm.y + 4), ACCENT);
        // Tiny monitors don't panic.
        let mut data = vec![0u8; 200 * 150 * 4];
        let mut canvas = Canvas::scaled(&mut data, 200, 150, 1.0);
        paint(&mut canvas, Text::load().as_ref(), &view(password("x", false)), 200, 150);
    }
}
