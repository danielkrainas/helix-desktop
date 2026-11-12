//! The on-screen keyboard (helix-desktop-spec
//! `contracts/gsettings/screen-keyboard.md`): shown while a text field
//! has focus and `screen-keyboard-enabled` is on; types through the input
//! method.

use helix_draw::Image;
use xkbcommon::xkb;

use super::*;

const ROW_H: i32 = 52;
/// The suggestion bar above the keys.
const BAR_H: i32 = 40;
/// How many suggestions the bar shows.
const SUGGESTIONS: usize = 3;
/// Holding a letter this long offers its accents.
const LONG_PRESS: std::time::Duration = std::time::Duration::from_millis(450);
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
    /// The layout's third-level (AltGr) characters.
    AltGr,
}

/// Row 5 on `page`; with `altgr`, an AltGr key too.
fn control_row(page: Page, altgr: bool) -> Vec<Key> {
    let flip = |p: Page| if page == p { Page::Letters } else { p };
    let mut row = vec![special(Special::Hide, 1.5), special(Special::Page(flip(Page::Symbols)), 1.5)];
    if altgr {
        row.push(special(Special::Page(flip(Page::AltGr)), 1.5));
    }
    row.extend([
        special(Special::Page(flip(Page::Emoji)), 1.5),
        special(Special::Space, if altgr { 4.5 } else { 6.0 }),
        special(Special::Left, 1.0),
        special(Special::Right, 1.0),
    ]);
    row
}

fn char_keys(chars: &str) -> Vec<Key> {
    chars.chars().filter(|c| !c.is_whitespace()).map(|c| Key { kind: KeyKind::Char(c.to_string(), c.to_string()), width: 1.0 }).collect()
}

const EMOJI: [&str; 3] = ["😀😂😊😍😘😎🤔😢😭😡", "👍👎👏🙏💪👋🤝✌👌🙌", "❤💔⭐🔥🎉✅❌⚠💡☕"];

/// On the AltGr page, characters the font cannot draw are left out.
fn drawable_rows(mut rows: Vec<Vec<Key>>, page: Page, text: Option<&Text>) -> Vec<Vec<Key>> {
    if let (Page::AltGr, Some(text)) = (page, text) {
        for row in &mut rows {
            row.retain(|k| !matches!(&k.kind, KeyKind::Char(n, _) if !text.covers(n)));
        }
    }
    rows
}

/// The rows of a page other than the letters, for `layout` and `variant`.
pub(super) fn page_rows(page: Page, layout: &str, variant: &str) -> Vec<Vec<Key>> {
    let altgr = altgr_rows(layout, variant).is_some();
    match page {
        Page::Letters => layout_rows(layout, variant),
        Page::AltGr => altgr_rows(layout, variant).unwrap_or_else(|| layout_rows(layout, variant)),
        Page::Symbols => {
            let mut row1 = char_keys("1234567890");
            row1.push(special(Special::Backspace, 2.0));
            let mut row2 = vec![special(Special::Tab, 1.5)];
            row2.extend(char_keys("@#$%&*-+()/"));
            let mut row3 = char_keys("!\"':;,.?_=");
            row3.push(special(Special::Enter, 2.0));
            let mut row4 = char_keys("~`|\\<>[]{}^");
            // The layout's currency, then the euro.
            let money = crate::osk_words::currency(layout);
            row4.push(Key { kind: KeyKind::Char(money.into(), money.into()), width: if money.chars().count() > 1 { 1.5 } else { 1.0 } });
            if money != "€" {
                row4.extend(char_keys("€"));
            }
            vec![row1, row2, row3, row4, control_row(Page::Symbols, altgr)]
        }
        Page::Emoji => {
            let mut rows: Vec<Vec<Key>> = EMOJI.iter().map(|r| char_keys(r)).collect();
            rows[0].push(special(Special::Backspace, 2.0));
            rows.push(vec![special(Special::Enter, 2.0)]);
            rows.push(control_row(Page::Emoji, altgr));
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

fn keymap(layout: &str, variant: &str) -> Option<xkb::Keymap> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    xkb::Keymap::new_from_names(&context, "evdev", "pc105", layout, variant, None, xkb::KEYMAP_COMPILE_NO_FLAGS)
        .or_else(|| xkb::Keymap::new_from_names(&context, "evdev", "pc105", "us", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS))
}

/// The character evdev key `code` gives at shift level `level`, if any.
fn level_char(keymap: Option<&xkb::Keymap>, code: u32, level: u32) -> Option<String> {
    let syms = keymap?.key_get_syms_by_level(xkb::Keycode::new(code + 8), 0, level);
    syms.first().map(|s| xkb::keysym_to_utf8(*s).trim_end_matches('\0').to_owned()).filter(|t| !t.is_empty() && !t.chars().any(char::is_control))
}

/// The main block's keys, by row.
const MAIN_ROWS: [&[u32]; 4] = [&[41, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13], &[16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 43], &[30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40], &[86, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53]];

/// The AltGr page: the main block's third-level characters (fourth with
/// Shift), when the layout has at least four of them.
pub(super) fn altgr_rows(layout: &str, variant: &str) -> Option<Vec<Vec<Key>>> {
    let keymap = keymap(layout, variant);
    let mut rows: Vec<Vec<Key>> = MAIN_ROWS
        .iter()
        .map(|codes| {
            let mut seen: Vec<String> = Vec::new();
            codes
                .iter()
                .filter_map(|&code| {
                    let third = level_char(keymap.as_ref(), code, 2)?;
                    // Some layouts repeat the plain character there.
                    if level_char(keymap.as_ref(), code, 0).as_deref() == Some(third.as_str()) || seen.contains(&third) {
                        return None;
                    }
                    seen.push(third.clone());
                    let fourth = level_char(keymap.as_ref(), code, 3).unwrap_or_else(|| third.to_uppercase());
                    Some(Key { kind: KeyKind::Char(third, fourth), width: 1.0 })
                })
                .collect()
        })
        .collect();
    if rows.iter().map(Vec::len).sum::<usize>() < 4 {
        return None;
    }
    rows[0].push(special(Special::Backspace, 2.0));
    rows[2].push(special(Special::Enter, 2.0));
    rows[3].insert(0, special(Special::Shift, 1.5));
    rows.push(control_row(Page::AltGr, true));
    Some(rows)
}

/// The keyboard's rows for an xkb layout and variant.
pub(super) fn layout_rows(layout: &str, variant: &str) -> Vec<Vec<Key>> {
    let keymap = keymap(layout, variant);
    let chars = |code: u32| -> Option<(String, String)> {
        let normal = level_char(keymap.as_ref(), code, 0)?;
        let shifted = level_char(keymap.as_ref(), code, 1).unwrap_or_else(|| normal.to_uppercase());
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
    let altgr = altgr_rows(layout, variant).is_some();
    vec![row1, row2, row3, row4, control_row(Page::Letters, altgr)]
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
    /// The suggestion bar: its words, and their places.
    bar: Vec<String>,
    bar_hits: Vec<(Rect, usize)>,
    /// The bar offers accents of a letter instead of suggestions.
    accents: bool,
    /// Letters typed on the keyboard since the last word ended (for text
    /// fields that do not tell their text).
    typed: String,
    /// The character key held down, and since when.
    pressed: Option<((usize, usize), std::time::Instant)>,
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
    /// The word list of the language last used, by language.
    pub dictionary: Option<(String, std::sync::Arc<crate::osk_words::Dictionary>)>,
    /// Words typed this session (never saved).
    pub learned: crate::osk_words::Learned,
}

fn height() -> u32 {
    (BAR_H + 5 * ROW_H + 2 * PAD) as u32
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
                self.osk_dictionary(&layout.0);
                self.osk.view = Some(Osk {
                    layer,
                    width: 0,
                    height: 0,
                    configured: false,
                    layout,
                    rows,
                    page: Page::Letters,
                    shift: false,
                    hits: Vec::new(),
                    bar: Vec::new(),
                    bar_hits: Vec::new(),
                    accents: false,
                    typed: String::new(),
                    pressed: None,
                });
            }
            (false, true) => self.osk.view = None,
            (true, true) => {
                // Follow the input source.
                let layout = active_layout();
                if self.osk.view.as_ref().is_some_and(|v| v.layout != layout) {
                    self.osk_dictionary(&layout.0);
                    if let Some(view) = self.osk.view.as_mut() {
                        view.rows = drawable_rows(page_rows(view.page, &layout.0, &layout.1), view.page, self.text.as_ref());
                        view.layout = layout;
                        view.typed.clear();
                    }
                    self.osk_text_changed();
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
        // The suggestion bar: up to three words (or a letter's accents),
        // side by side over the keys' width.
        view.bar_hits.clear();
        if !view.bar.is_empty() {
            let keys_w = (widest * unit).min(w as f32 - 2.0 * PAD as f32);
            let left = (w as f32 - keys_w) / 2.0;
            let cell = keys_w / view.bar.len().max(SUGGESTIONS) as f32;
            for (i, word) in view.bar.iter().enumerate() {
                let r = Rect { x: (left + i as f32 * cell) as i32 + GAP / 2, y: PAD / 2 + GAP / 2, w: cell as i32 - GAP, h: BAR_H - GAP };
                if i > 0 && !view.accents {
                    canvas.rect(r.x - GAP / 2, r.y + 8, 1, r.h - 16, BANNER_BORDER);
                }
                if view.accents {
                    canvas.rect(r.x, r.y, r.w, r.h, FIELD_BG);
                }
                let px = 16.0;
                let tw = text.map_or(0.0, |t| t.width(word, px)).min(r.w as f32);
                label(text, &mut canvas, word, r.x as f32 + (r.w as f32 - tw) / 2.0, baseline(text, r.y, r.h, px), px, FG, r.w as f32);
                view.bar_hits.push((r, i));
            }
        }
        view.hits.clear();
        for (ri, row) in view.rows.iter().enumerate() {
            let row_w: f32 = row.iter().map(|k| k.width).sum::<f32>() * unit;
            let mut x = (w as f32 - row_w) / 2.0;
            let y = BAR_H + PAD + ri as i32 * ROW_H;
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
                    KeyKind::Special(Special::Page(Page::AltGr)) => Some("AltGr".into()),
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

    /// Load the word list for the language `layout` types (once per
    /// language).
    fn osk_dictionary(&mut self, layout: &str) {
        let language = crate::osk_words::language_of_layout(layout);
        if self.osk.dictionary.as_ref().is_some_and(|(l, _)| l == language) {
            return;
        }
        let locale = std::env::var("LC_ALL").or_else(|_| std::env::var("LC_MESSAGES")).or_else(|_| std::env::var("LANG")).unwrap_or_default();
        let path = crate::osk_words::dictionary_path(language, &locale, &crate::osk_words::dictionary_dirs());
        let dictionary = path.as_deref().and_then(crate::osk_words::Dictionary::load_hunspell).unwrap_or_default();
        tracing::info!(language, path = ?path, words = dictionary.len(), "on-screen keyboard word list");
        self.osk.dictionary = Some((language.to_owned(), std::sync::Arc::new(dictionary)));
    }

    /// The word before the cursor: from the text field when it tells,
    /// else what was typed on the keyboard.
    fn osk_word(&self) -> String {
        match self.ime_surrounding() {
            Some((text, cursor)) => crate::osk_words::word_before(&text, cursor as usize).to_owned(),
            None => self.osk.view.as_ref().map(|v| v.typed.clone()).unwrap_or_default(),
        }
    }

    /// Refresh the suggestions after the text changed.
    pub(super) fn osk_text_changed(&mut self) {
        let Some(view) = self.osk.view.as_ref() else { return };
        if view.accents {
            return;
        }
        let (hint, purpose) = self.ime_content();
        let allowed = view.page == Page::Letters && crate::osk_words::suggestions_allowed(hint, purpose);
        let words = match (&self.osk.dictionary, allowed) {
            (Some((_, dictionary)), true) => crate::osk_words::suggest(dictionary, &self.osk.learned, &self.osk_word(), SUGGESTIONS),
            _ => Vec::new(),
        };
        if let Some(view) = self.osk.view.as_mut().filter(|v| v.bar != words) {
            view.bar = words;
            self.draw_osk();
        }
    }

    /// A word ended: remember it (not in fields that refuse suggestions).
    fn osk_word_ended(&mut self) {
        let (hint, purpose) = self.ime_content();
        if crate::osk_words::suggestions_allowed(hint, purpose) {
            let word = self.osk_word();
            self.osk.learned.add(&word);
        }
        if let Some(view) = self.osk.view.as_mut() {
            view.typed.clear();
        }
    }

    /// Type a character key's text.
    fn osk_type(&mut self, text: &str) {
        let letter = text.chars().all(|c| c.is_alphabetic() || c == '\'');
        if !letter {
            self.osk_word_ended();
        }
        self.ime_commit_text(text);
        let surrounding = self.ime_surrounding().is_some();
        if let Some(view) = self.osk.view.as_mut() {
            if letter {
                view.typed.push_str(text);
            }
        }
        // Text fields that tell their text answer with it; the others are
        // followed here.
        if !surrounding {
            self.osk_text_changed();
        }
    }

    /// The suggestion bar was clicked.
    fn osk_bar(&mut self, index: usize) {
        let Some(view) = self.osk.view.as_mut() else { return };
        let Some(choice) = view.bar.get(index).cloned() else { return };
        if view.accents {
            view.accents = false;
            view.bar.clear();
            self.osk_type(&choice);
            self.osk_text_changed();
            self.draw_osk();
            return;
        }
        let word = self.osk_word();
        tracing::debug!(word, choice, "suggestion picked");
        self.ime_replace_before(word.len() as u32, &format!("{choice} "));
        self.osk.learned.add(&choice);
        if let Some(view) = self.osk.view.as_mut() {
            view.typed.clear();
            view.bar.clear();
        }
        self.draw_osk();
    }

    /// Pointer input on the keyboard; true if it was for it.
    pub(super) fn osk_pointer(&mut self, event: &PointerEvent) -> bool {
        let Some(view) = self.osk.view.as_mut().filter(|v| v.layer.wl_surface() == &event.surface) else { return false };
        let (x, y) = event.position;
        let at = view.hits.iter().find(|(r, _)| r.contains(x, y)).map(|h| h.1);
        match event.kind {
            PointerEventKind::Press { button: BTN_LEFT, .. } => {}
            PointerEventKind::Release { button: BTN_LEFT, .. } => {
                // A character key types when let go; held long, it offers
                // its accents instead.
                let Some((held, since)) = view.pressed.take() else { return true };
                let Some(KeyKind::Char(normal, shifted)) = view.rows.get(held.0).and_then(|r| r.get(held.1)).map(|k| k.kind.clone()) else { return true };
                let shift = view.shift;
                let text = if shift { shifted } else { normal };
                let accents = crate::osk_words::accents_for(&text);
                if since.elapsed() >= LONG_PRESS && !accents.is_empty() {
                    view.shift = false;
                    view.accents = true;
                    view.bar = accents;
                    self.draw_osk();
                    return true;
                }
                if at != Some(held) {
                    return true;
                }
                view.shift = false;
                self.osk_type(&text);
                if shift {
                    self.draw_osk();
                }
                return true;
            }
            _ => return true,
        }
        if let Some(&(_, index)) = view.bar_hits.iter().find(|(r, _)| r.contains(x, y)) {
            self.osk_bar(index);
            return true;
        }
        let Some((ri, ki)) = at else { return true };
        let Some(key) = view.rows.get(ri).and_then(|r| r.get(ki)).cloned() else { return true };
        if view.accents {
            // Any other key puts the suggestions back.
            view.accents = false;
            view.bar.clear();
        }
        match key.kind {
            KeyKind::Char(..) => view.pressed = Some(((ri, ki), std::time::Instant::now())),
            KeyKind::Special(Special::Shift) => {
                view.shift = !view.shift;
                self.draw_osk();
            }
            KeyKind::Special(Special::Space) => {
                self.osk_word_ended();
                self.ime_commit_text(" ");
                self.osk_text_changed();
            }
            KeyKind::Special(Special::Page(page)) => {
                view.page = page;
                view.shift = false;
                view.rows = drawable_rows(page_rows(page, &view.layout.0, &view.layout.1), page, self.text.as_ref());
                view.bar.clear();
                self.osk_text_changed();
                self.draw_osk();
            }
            KeyKind::Special(Special::Hide) => {
                self.osk.dismissed = true;
                self.osk.view = None;
            }
            KeyKind::Special(s) => {
                match s {
                    Special::Backspace => {
                        view.typed.pop();
                    }
                    _ => self.osk_word_ended(),
                }
                if let Some(code) = s.evdev() {
                    self.ime_tap_key(code);
                }
                if self.ime_surrounding().is_none() {
                    self.osk_text_changed();
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
        let symbols = page_rows(Page::Symbols, "us", "");
        assert_eq!(chars(&symbols[0]), "1234567890");
        assert_eq!(chars(&symbols[3]), "~`|\\<>[]{}^$€", "the layout's currency, then the euro");
        assert!(symbols[4].contains(&special(Special::Page(Page::Letters), 1.5)), "ABC back");
        assert_eq!(chars(&page_rows(Page::Symbols, "gb", "")[3]), "~`|\\<>[]{}^£€");
        assert_eq!(chars(&page_rows(Page::Symbols, "de", "")[3]), "~`|\\<>[]{}^€");
        let emoji = page_rows(Page::Emoji, "us", "");
        assert_eq!(emoji.len(), 5);
        assert_eq!(emoji[0].iter().filter(|k| matches!(k.kind, KeyKind::Char(..))).count(), 10);
    }

    #[test]
    fn altgr_pages() {
        assert!(altgr_rows("us", "").is_none(), "us has no third level");
        let de = altgr_rows("de", "").expect("German has AltGr characters");
        assert!(chars(&de[1]).starts_with("@"), "AltGr+Q is @: {}", chars(&de[1]));
        assert!(chars(&de[1]).contains('€'));
        assert!(chars(&de[0]).contains('{') && chars(&de[0]).contains('}'));
        let letters = layout_rows("de", "");
        assert!(letters[4].contains(&special(Special::Page(Page::AltGr), 1.5)), "an AltGr key");
        assert!(!layout_rows("us", "")[4].contains(&special(Special::Page(Page::AltGr), 1.5)));
        assert!(de[4].contains(&special(Special::Page(Page::Letters), 1.5)), "ABC back");
    }
}
