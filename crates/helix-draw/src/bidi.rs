//! Right-to-left text and layouts (helix-desktop-spec
//! `components/i18n.md`, "Right-to-left languages").
//!
//! - [`set_rtl`]: the interface reads right to left (the shell and the
//!   greeter set it from the translation at start).
//! - [`visual`]: a line of text in display order (the Unicode
//!   Bidirectional Algorithm), so Hebrew or Arabic words read right to
//!   left inside any layout. Text without right-to-left letters comes
//!   back as it is.
//! - A [`crate::Canvas`] made [`crate::Canvas::mirrored`] draws its whole
//!   layout mirrored, with text and pictures kept readable.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};

use unicode_bidi::{get_base_direction, Direction, Level, ParagraphBidiInfo};

static RTL: AtomicBool = AtomicBool::new(false);

/// The interface reads right to left.
pub fn set_rtl(on: bool) {
    RTL.store(on, Ordering::Relaxed);
}

pub fn rtl() -> bool {
    RTL.load(Ordering::Relaxed)
}

/// Whether `c` is written right to left, or forces that direction.
fn is_rtl_char(c: char) -> bool {
    matches!(c as u32,
        0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF
        | 0x200F | 0x202B | 0x202E | 0x2067)
}

/// Whether `text` has right-to-left letters.
pub fn has_rtl(text: &str) -> bool {
    text.chars().any(is_rtl_char)
}

/// The direction a line of `text` reads in: that of its first strong
/// letter; the interface's for text without letters (numbers, symbols).
pub fn line_is_rtl(text: &str) -> bool {
    match get_base_direction(text) {
        Direction::Ltr => false,
        Direction::Rtl => true,
        Direction::Mixed => rtl(),
    }
}

/// The mirror image of a paired punctuation mark, for right-to-left runs.
fn mirror(c: char) -> char {
    match c {
        '(' => ')',
        ')' => '(',
        '[' => ']',
        ']' => '[',
        '{' => '}',
        '}' => '{',
        '<' => '>',
        '>' => '<',
        '«' => '»',
        '»' => '«',
        '‹' => '›',
        '›' => '‹',
        other => other,
    }
}

/// A single line of `text` in visual (left-to-right display) order.
pub fn visual(text: &str) -> Cow<'_, str> {
    if !has_rtl(text) {
        return Cow::Borrowed(text);
    }
    let level = if line_is_rtl(text) { Level::rtl() } else { Level::ltr() };
    let info = ParagraphBidiInfo::new(text, Some(level));
    let (levels, runs) = info.visual_runs(0..text.len());
    let mut out = String::with_capacity(text.len());
    for run in runs {
        let part = &text[run.clone()];
        if levels[run.start].is_rtl() {
            out.extend(part.chars().rev().map(mirror));
        } else {
            out.push_str(part);
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_order() {
        assert_eq!(visual("Files"), "Files");
        // Hebrew letters read right to left: stored first, drawn last.
        assert_eq!(visual("אבג"), "גבא");
        // Numbers inside right-to-left text keep their order.
        assert_eq!(visual("אב 12"), "12 בא");
        // Brackets are mirrored in right-to-left runs.
        assert_eq!(visual("א (ב)"), "(ב) א");
        // A Latin word in a right-to-left line stays readable.
        assert_eq!(visual("אב Helix גד"), "דג Helix בא");
        assert!(line_is_rtl("אב Helix") && !line_is_rtl("Helix אב"));
    }
}
