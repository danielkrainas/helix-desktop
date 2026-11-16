//! The login screen's own on-screen keyboard (helixdesktop/spec
//! `components/greeter.md`, "Accessibility"). The greeter only types
//! into its own fields, so its keys are keys of the greeter
//! ([`crate::app::Key`]), not of a virtual keyboard.

use helix_i18n::tr;

use crate::app::Key;

/// Which keys are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    Letters,
    /// Digits and punctuation (Shift: the rest of the symbols).
    Symbols,
}

/// What a key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Char(char),
    Shift,
    Backspace,
    Space,
    Return,
    /// Show that page.
    Page(Page),
}

/// A key, `width` units wide (a letter is one unit; a row is ten).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyDef {
    pub action: Action,
    pub width: f32,
}

/// The keyboard's state: the page and Shift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Osk {
    pub page: Page,
    /// Letters: the next letter is a capital (Shift is let go after it).
    /// Symbols: the second set of symbols is shown (until pressed again).
    pub shift: bool,
}

const LETTERS: [&str; 3] = ["qwertyuiop", "asdfghjkl", "zxcvbnm"];
const SYMBOLS: [&str; 3] = ["1234567890", "@#$%&-+=()", "*!':;/?"];
/// With Shift on the symbols page: the rest of ASCII's punctuation and a
/// few common signs.
const MORE: [&str; 3] = ["~`|\\^_{}[]", "<>\"€£¥°§·×", "÷¢¡¿«»¶"];

fn chars(row: &str, upper: bool) -> impl Iterator<Item = KeyDef> + '_ {
    row.chars().map(move |c| KeyDef { action: Action::Char(if upper { c.to_ascii_uppercase() } else { c }), width: 1.0 })
}

impl Osk {
    /// The rows of keys, top to bottom.
    pub fn rows(&self) -> Vec<Vec<KeyDef>> {
        let (set, upper) = match (self.page, self.shift) {
            (Page::Letters, shift) => (LETTERS, shift),
            (Page::Symbols, false) => (SYMBOLS, false),
            (Page::Symbols, true) => (MORE, false),
        };
        let wide = |action| KeyDef { action, width: 1.5 };
        let one = |action| KeyDef { action, width: 1.0 };
        let mut third = vec![wide(Action::Shift)];
        third.extend(chars(set[2], upper));
        third.push(wide(Action::Backspace));
        let other = if self.page == Page::Letters { Page::Symbols } else { Page::Letters };
        vec![
            chars(set[0], upper).collect(),
            chars(set[1], upper).collect(),
            third,
            vec![wide(Action::Page(other)), one(Action::Char(',')), KeyDef { action: Action::Space, width: 5.0 }, one(Action::Char('.')), wide(Action::Return)],
        ]
    }

    /// Every key, row by row (what `Hit::OskKey(n)` counts).
    pub fn keys(&self) -> Vec<KeyDef> {
        self.rows().into_iter().flatten().collect()
    }

    /// Press key `index`: what it types, if anything.
    pub fn press(&mut self, index: usize) -> Option<Key> {
        let key = self.keys().get(index)?.action;
        match key {
            Action::Char(c) => {
                if self.page == Page::Letters {
                    self.shift = false;
                }
                Some(Key::Text(c.to_string()))
            }
            Action::Shift => {
                self.shift = !self.shift;
                None
            }
            Action::Backspace => Some(Key::Backspace),
            Action::Space => Some(Key::Text(" ".into())),
            Action::Return => Some(Key::Return),
            Action::Page(page) => {
                *self = Self { page, shift: false };
                None
            }
        }
    }
}

/// The words on a key (`None`: a symbol is drawn instead, or nothing).
pub fn label(action: Action) -> Option<String> {
    match action {
        Action::Char(c) => Some(c.to_string()),
        Action::Page(Page::Symbols) => Some("?123".into()),
        Action::Page(Page::Letters) => Some("ABC".into()),
        _ => None,
    }
}

/// A key's accessible name.
pub fn name(action: Action) -> String {
    match action {
        Action::Char(c) => c.to_string(),
        Action::Shift => tr("Shift").into(),
        Action::Backspace => tr("Backspace").into(),
        Action::Space => tr("Space").into(),
        Action::Return => tr("Return").into(),
        Action::Page(Page::Symbols) => tr("Symbols").into(),
        Action::Page(Page::Letters) => tr("Letters").into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(osk: &Osk, action: Action) -> usize {
        osk.keys().iter().position(|k| k.action == action).unwrap()
    }

    #[test]
    fn rows_are_ten_units() {
        for osk in [Osk::default(), Osk { page: Page::Letters, shift: true }, Osk { page: Page::Symbols, shift: false }, Osk { page: Page::Symbols, shift: true }] {
            let rows = osk.rows();
            assert_eq!(rows.len(), 4);
            for row in &rows {
                let units: f32 = row.iter().map(|k| k.width).sum();
                assert!(units <= 10.0, "{osk:?}: {units}");
            }
        }
    }

    /// Every printable ASCII character can be typed.
    #[test]
    fn all_of_ascii() {
        let mut typed = String::new();
        for osk in [Osk::default(), Osk { page: Page::Letters, shift: true }, Osk { page: Page::Symbols, shift: false }, Osk { page: Page::Symbols, shift: true }] {
            for k in osk.keys() {
                if let Action::Char(c) = k.action {
                    typed.push(c);
                }
            }
        }
        for c in (0x21u8..0x7F).map(char::from) {
            assert!(typed.contains(c), "{c:?} cannot be typed");
        }
    }

    #[test]
    fn shift_and_pages() {
        let mut osk = Osk::default();
        assert_eq!(osk.press(0), Some(Key::Text("q".into())));
        assert_eq!(osk.press(index(&osk, Action::Shift)), None);
        assert_eq!(osk.press(0), Some(Key::Text("Q".into())), "a capital");
        assert_eq!(osk.press(0), Some(Key::Text("q".into())), "Shift let go");
        assert_eq!(osk.press(index(&osk, Action::Backspace)), Some(Key::Backspace));
        assert_eq!(osk.press(index(&osk, Action::Space)), Some(Key::Text(" ".into())));
        assert_eq!(osk.press(index(&osk, Action::Return)), Some(Key::Return));
        osk.press(index(&osk, Action::Page(Page::Symbols)));
        assert_eq!(osk.press(0), Some(Key::Text("1".into())));
        osk.press(index(&osk, Action::Shift));
        assert_eq!(osk.press(0), Some(Key::Text("~".into())));
        assert_eq!(osk.press(1), Some(Key::Text("`".into())), "Shift stays on the symbols page");
        osk.press(index(&osk, Action::Page(Page::Letters)));
        assert_eq!(osk, Osk::default());
        assert_eq!(osk.press(999), None);
    }
}
