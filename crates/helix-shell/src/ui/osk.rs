//! The on-screen keyboard (helix-desktop-spec
//! `contracts/gsettings/screen-keyboard.md`): shown while a text field
//! has focus and `screen-keyboard-enabled` is on; types through the input
//! method.

use helix_draw::Image;
use xkbcommon::xkb;

use super::*;

const ROW_H: i32 = 52;
const PAD: i32 = 8;
const GAP: i32 = 6;
const MAX_UNIT: f32 = 76.0;
const A11Y_APPS: &str = "org.gnome.desktop.a11y.applications";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Special {
    Backspace,
    Enter,
    Tab,
    Shift,
    Space,
    Left,
    Right,
    Hide,
    /// Switch to a page.
    Page(Page),
}

/// The keyboard's pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Page {
    #[default]
    Letters,
    Symbols,
    Emoji,
}

/// Row 5 on `page`.
fn control_row(page: Page) -> Vec<Key> {
    let symbols = if page == Page::Symbols { Page::Letters } else { Page::Symbols };
    let emoji = if page == Page::Emoji { Page::Letters } else { Page::Emoji };
    vec![
        special(Special::Hide, 1.5),
        special(Special::Page(symbols), 1.5),
        special(Special::Page(emoji), 1.5),
        special(Special::Space, 6.0),
        special(Special::Left, 1.0),
        special(Special::Right, 1.0),
    ]
}

fn char_keys(chars: &str) -> Vec<Key> {
    chars.chars().filter(|c| !c.is_whitespace()).map(|c| Key { kind: KeyKind::Char(c.to_string(), c.to_string()), width: 1.0 }).collect()
}

const EMOJI: [&str; 3] = ["😀😂😊😍😘😎🤔😢😭😡", "👍👎👏🙏💪👋🤝✌👌🙌", "❤💔⭐🔥🎉✅❌⚠💡☕"];

/// The rows of a page other than the letters.
pub(super) fn page_rows(page: Page) -> Vec<Vec<Key>> {
    match page {
        Page::Letters => Vec::new(),
        Page::Symbols => {
            let mut row1 = char_keys("1234567890");
            row1.push(special(Special::Backspace, 2.0));
            let mut row2 = vec![special(Special::Tab, 1.5)];
            row2.extend(char_keys("@#$%&*-+()/"));
            let mut row3 = char_keys("!\"':;,.?_=");
            row3.push(special(Special::Enter, 2.0));
            let row4 = char_keys("~`|\\<>[]{}^€");
            vec![row1, row2, row3, row4, control_row(Page::Symbols)]
        }
        Page::Emoji => {
            let mut rows: Vec<Vec<Key>> = EMOJI.iter().map(|r| char_keys(r)).collect();
            rows[0].push(special(Special::Backspace, 2.0));
            rows.push(vec![special(Special::Enter, 2.0)]);
            rows.push(control_row(Page::Emoji));
            rows
        }
    }
}

impl Special {
    /// The evdev key it presses, for keys sent as key presses.
    fn evdev(self) -> Option<u32> {
        match self {
            Special::Backspace => Some(14),
            Special::Enter => Some(28),
            Special::Tab => Some(15),
            Special::Left => Some(105),
            Special::Right => Some(106),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum KeyKind {
    /// Its characters without and with Shift.
    Char(String, String),
    Special(Special),
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Key {
    pub kind: KeyKind,
    /// In key widths.
    pub width: f32,
}

fn special(s: Special, width: f32) -> Key {
    Key { kind: KeyKind::Special(s), width }
}

/// The keyboard's rows for an xkb layout and variant.
pub(super) fn layout_rows(layout: &str, variant: &str) -> Vec<Vec<Key>> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(&context, "evdev", "pc105", layout, variant, None, xkb::KEYMAP_COMPILE_NO_FLAGS)
        .or_else(|| xkb::Keymap::new_from_names(&context, "evdev", "pc105", "us", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS));
    let chars = |code: u32| -> Option<(String, String)> {
        let keymap = keymap.as_ref()?;
        let level = |l| {
            let syms = keymap.key_get_syms_by_level(xkb::Keycode::new(code + 8), 0, l);
            syms.first().map(|s| xkb::keysym_to_utf8(*s).trim_end_matches('\0').to_owned()).filter(|t| !t.is_empty() && !t.chars().any(char::is_control))
        };
        let normal = level(0)?;
        let shifted = level(1).unwrap_or_else(|| normal.to_uppercase());
        Some((normal, shifted))
    };
    let keys = |codes: &[u32]| -> Vec<Key> { codes.iter().filter_map(|c| chars(*c)).map(|(n, s)| Key { kind: KeyKind::Char(n, s), width: 1.0 }).collect() };
    let mut row1 = keys(&[41, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
    row1.push(special(Special::Backspace, 2.0));
    let mut row2 = vec![special(Special::Tab, 1.5)];
    row2.extend(keys(&[16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 43]));
    let mut row3 = keys(&[30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40]);
    row3.push(special(Special::Enter, 2.0));
    let mut row4 = vec![special(Special::Shift, 1.5)];
    // The ISO key, when its characters are on no other key.
    let others: Vec<String> = [&row1, &row2, &row3]
        .iter()
        .flat_map(|r| r.iter())
        .chain(keys(&[44, 45, 46, 47, 48, 49, 50, 51, 52, 53]).iter())
        .filter_map(|k| if let KeyKind::Char(n, s) = &k.kind { Some([n.clone(), s.clone()]) } else { None })
        .flatten()
        .collect();
    if chars(86).is_some_and(|(n, s)| !others.contains(&n) && !others.contains(&s)) {
        row4.extend(keys(&[86]));
    }
    row4.extend(keys(&[44, 45, 46, 47, 48, 49, 50, 51, 52, 53]));
    row4.push(special(Special::Shift, 1.5));
    vec![row1, row2, row3, row4, control_row(Page::Letters)]
}

/// The layout and variant of the active input source (`us` for others).
fn active_layout() -> (String, String) {
    match crate::input_sources::active() {
        Some((kind, id)) if kind == "xkb" => {
            let (layout, variant) = id.split_once('+').unwrap_or((id.as_str(), ""));
            (layout.to_owned(), variant.to_owned())
        }
        _ => ("us".into(), String::new()),
    }
}

fn enabled() -> bool {
    use gio::prelude::*;
    helix_settings::has_schema(A11Y_APPS) && gio::Settings::new(A11Y_APPS).boolean("screen-keyboard-enabled")
}

pub(super) struct Osk {
    layer: LayerSurface,
    width: u32,
    height: u32,
    configured: bool,
    layout: (String, String),
    rows: Vec<Vec<Key>>,
    page: Page,
    shift: bool,
    hits: Vec<(Rect, (usize, usize))>,
}

/// What the keyboard keeps between showings.
#[derive(Default)]
pub(super) struct OskState {
    pub view: Option<Osk>,
    /// "Hide" was pressed: stay hidden until a text field is focused again.
    pub dismissed: bool,
    pub was_typing: bool,
    /// The emoji page's font (looked for once).
    pub emoji_font: Option<Option<Text>>,
}

fn height() -> u32 {
    (5 * ROW_H + 2 * PAD) as u32
}

impl Ui {
    /// Show or hide the keyboard as the setting and the text focus say.
    pub(super) fn osk_sync(&mut self) {
        let typing = self.ime_typing();
        if typing && !self.osk.was_typing {
            self.osk.dismissed = false;
        }
        self.osk.was_typing = typing;
        let want = typing && !self.osk.dismissed && enabled();
        match (want, self.osk.view.is_some()) {
            (true, false) => {
                let surface = self.compositor.create_surface(&self.qh);
                let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Top, Some("helix-osk"), None);
                layer.set_anchor(Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
                layer.set_size(0, height());
                layer.set_exclusive_zone(height() as i32);
                layer.set_keyboard_interactivity(KeyboardInteractivity::None);
                layer.commit();
                let layout = active_layout();
                let rows = layout_rows(&layout.0, &layout.1);
                tracing::info!(layout = ?layout, "on-screen keyboard shown");
                self.osk.view = Some(Osk { layer, width: 0, height: 0, configured: false, layout, rows, page: Page::Letters, shift: false, hits: Vec::new() });
            }
            (false, true) => self.osk.view = None,
            (true, true) => {
                // Follow the input source.
                let layout = active_layout();
                if let Some(view) = self.osk.view.as_mut().filter(|v| v.layout != layout) {
                    if view.page == Page::Letters {
                        view.rows = layout_rows(&layout.0, &layout.1);
                    }
                    view.layout = layout;
                    self.draw_osk();
                }
            }
            (false, false) => {}
        }
    }

    pub(super) fn osk_configured(&mut self, layer: &LayerSurface, size: (u32, u32)) -> bool {
        let Some(view) = self.osk.view.as_mut().filter(|v| &v.layer == layer) else { return false };
        view.width = size.0;
        view.height = size.1;
        view.configured = true;
        self.draw_osk();
        true
    }

    pub(super) fn osk_closed(&mut self, layer: &LayerSurface) -> bool {
        if self.osk.view.as_ref().is_some_and(|v| &v.layer == layer) {
            self.osk.view = None;
            return true;
        }
        false
    }

    fn draw_osk(&mut self) {
        let Some(mut view) = self.osk.view.take() else { return };
        if view.configured && view.width > 0 {
            self.paint_osk(&mut view);
        }
        self.osk.view = Some(view);
    }

    fn paint_osk(&mut self, view: &mut Osk) {
        let (w, h) = (view.width as usize, view.height as usize);
        let scale = self.scales.of(view.layer.wl_surface());
        let icons: Vec<(Special, Option<Image>)> = [(Special::Backspace, "edit-clear-symbolic"), (Special::Left, "go-previous-symbolic"), (Special::Right, "go-next-symbolic"), (Special::Hide, "input-keyboard-symbolic")]
            .into_iter()
            .map(|(s, name)| (s, self.symbolic_icon(name, scale)))
            .collect();
        let Ok((buffer, data)) =
            self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888)
        else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale);
        canvas.fill(PANEL_BG);
        canvas.rect(0, 0, w as i32, 1, BANNER_BORDER);
        let text = self.text.as_ref();
        let widest = view.rows.iter().map(|r| r.iter().map(|k| k.width).sum::<f32>()).fold(0.0, f32::max);
        if view.page == Page::Emoji && self.osk.emoji_font.is_none() {
            self.osk.emoji_font = Some(Text::load_matching("emoji:color=false"));
        }
        let emoji = self.osk.emoji_font.as_ref().and_then(|f| f.as_ref());
        let unit = ((w as f32 - 2.0 * PAD as f32) / widest).min(MAX_UNIT);
        view.hits.clear();
        for (ri, row) in view.rows.iter().enumerate() {
            let row_w: f32 = row.iter().map(|k| k.width).sum::<f32>() * unit;
            let mut x = (w as f32 - row_w) / 2.0;
            let y = PAD + ri as i32 * ROW_H;
            for (ki, key) in row.iter().enumerate() {
                let kw = key.width * unit;
                let r = Rect { x: x as i32 + GAP / 2, y: y + GAP / 2, w: kw as i32 - GAP, h: ROW_H - GAP };
                let lit = matches!(key.kind, KeyKind::Special(Special::Shift)) && view.shift;
                let dark = matches!(key.kind, KeyKind::Special(s) if s != Special::Space);
                canvas.rect(r.x, r.y, r.w, r.h, if lit { ACCENT } else if dark { BANNER_BG } else { FIELD_BG });
                let caption = match &key.kind {
                    KeyKind::Char(normal, shifted) => Some(if view.shift { shifted.clone() } else { normal.clone() }),
                    KeyKind::Special(Special::Shift) => Some("Shift".into()),
                    KeyKind::Special(Special::Enter) => Some("Enter".into()),
                    KeyKind::Special(Special::Tab) => Some("Tab".into()),
                    KeyKind::Special(Special::Page(Page::Letters)) => Some("ABC".into()),
                    KeyKind::Special(Special::Page(Page::Symbols)) => Some("?123".into()),
                    KeyKind::Special(Special::Page(Page::Emoji)) => Some("Emoji".into()),
                    KeyKind::Special(Special::Space) => None,
                    KeyKind::Special(s) => {
                        if let Some((_, Some(icon))) = icons.iter().find(|(x, _)| x == s) {
                            canvas.image(f64::from(r.x) + f64::from(r.w - 18) / 2.0, f64::from(r.y) + f64::from(r.h - 18) / 2.0, 18.0, 18.0, icon);
                        }
                        None
                    }
                };
                if let Some(caption) = caption {
                    let px = if matches!(key.kind, KeyKind::Char(..)) { 20.0 } else { 14.0 };
                    let font = if view.page == Page::Emoji && matches!(key.kind, KeyKind::Char(..)) { emoji } else { text };
                    let tw = font.map_or(0.0, |t| t.width(&caption, px));
                    label(font, &mut canvas, &caption, r.x as f32 + (r.w as f32 - tw) / 2.0, baseline(font, r.y, r.h, px), px, FG, r.w as f32);
                }
                view.hits.push((r, (ri, ki)));
                x += kw;
            }
        }
        let surface = view.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            view.layer.commit();
        }
    }

    /// Pointer input on the keyboard; true if it was for it.
    pub(super) fn osk_pointer(&mut self, event: &PointerEvent) -> bool {
        let Some(view) = self.osk.view.as_mut().filter(|v| v.layer.wl_surface() == &event.surface) else { return false };
        let PointerEventKind::Press { button: BTN_LEFT, .. } = event.kind else { return true };
        let (x, y) = event.position;
        let Some(&(_, (ri, ki))) = view.hits.iter().find(|(r, _)| r.contains(x, y)) else { return true };
        let Some(key) = view.rows.get(ri).and_then(|r| r.get(ki)).cloned() else { return true };
        match key.kind {
            KeyKind::Char(normal, shifted) => {
                let shift = view.shift;
                view.shift = false;
                self.ime_commit_text(if shift { &shifted } else { &normal });
                if shift {
                    self.draw_osk();
                }
            }
            KeyKind::Special(Special::Shift) => {
                view.shift = !view.shift;
                self.draw_osk();
            }
            KeyKind::Special(Special::Space) => self.ime_commit_text(" "),
            KeyKind::Special(Special::Page(page)) => {
                view.page = page;
                view.shift = false;
                view.rows = if page == Page::Letters { layout_rows(&view.layout.0, &view.layout.1) } else { page_rows(page) };
                self.draw_osk();
            }
            KeyKind::Special(Special::Hide) => {
                self.osk.dismissed = true;
                self.osk.view = None;
            }
            KeyKind::Special(s) => {
                if let Some(code) = s.evdev() {
                    self.ime_tap_key(code);
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(row: &[Key]) -> String {
        row.iter().filter_map(|k| if let KeyKind::Char(n, _) = &k.kind { Some(n.as_str()) } else { None }).collect()
    }

    #[test]
    fn layouts() {
        let us = layout_rows("us", "");
        assert_eq!(chars(&us[1]), "qwertyuiop[]\\");
        assert_eq!(chars(&us[2]), "asdfghjkl;'");
        assert_eq!(chars(&us[3]), "zxcvbnm,./", "no ISO key on us");
        assert!(matches!(&us[0][1].kind, KeyKind::Char(n, s) if n == "1" && s == "!"));
        let de = layout_rows("de", "");
        assert_eq!(chars(&de[1]), "qwertzuiopü+#");
        assert!(chars(&de[3]).starts_with("<y"), "the ISO key");
        assert!(!chars(&de[0]).contains('´'), "dead keys are left out");
        assert_eq!(us[4].len(), 6, "Hide, ?123, Emoji, Space, ←, →");
        assert!(us[4].contains(&special(Special::Page(Page::Symbols), 1.5)));
        let symbols = page_rows(Page::Symbols);
        assert_eq!(chars(&symbols[0]), "1234567890");
        assert_eq!(chars(&symbols[3]), "~`|\\<>[]{}^€");
        assert!(symbols[4].contains(&special(Special::Page(Page::Letters), 1.5)), "ABC back");
        let emoji = page_rows(Page::Emoji);
        assert_eq!(emoji.len(), 5);
        assert_eq!(emoji[0].iter().filter(|k| matches!(k.kind, KeyKind::Char(..))).count(), 10);
    }
}
