//! The on-screen keyboard's language data (helix-desktop-spec
//! `contracts/gsettings/screen-keyboard.md`, "Languages" and
//! "Suggestions"): which language a layout types, its word list, word
//! suggestions, accented letters and currency sign.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The language (ISO 639-1) an xkb layout is for; English for unknown ones.
pub fn language_of_layout(layout: &str) -> &'static str {
    match layout {
        "us" | "gb" | "au" | "ie" | "nz" | "za" => "en",
        "ca" => "fr",
        "de" | "at" | "ch" => "de",
        "fr" | "be" => "fr",
        "es" | "latam" => "es",
        "it" => "it",
        "pt" | "br" => "pt",
        "nl" => "nl",
        "se" => "sv",
        "no" => "nb",
        "dk" => "da",
        "fi" => "fi",
        "is" => "is",
        "pl" => "pl",
        "cz" => "cs",
        "sk" => "sk",
        "hu" => "hu",
        "ro" => "ro",
        "si" => "sl",
        "hr" => "hr",
        "rs" => "sr",
        "bg" => "bg",
        "ru" => "ru",
        "ua" => "uk",
        "by" => "be",
        "gr" => "el",
        "tr" => "tr",
        "ee" => "et",
        "lv" => "lv",
        "lt" => "lt",
        "il" => "he",
        "ara" => "ar",
        "ir" => "fa",
        "in" => "hi",
        "jp" => "ja",
        "kr" => "ko",
        "cn" => "zh",
        "vn" => "vi",
        "th" => "th",
        _ => "en",
    }
}

/// The currency sign a layout's users are most likely to want.
pub fn currency(layout: &str) -> &'static str {
    match layout {
        "us" | "ca" | "au" | "latam" => "$",
        "br" => "R$",
        "gb" => "£",
        "jp" | "cn" => "¥",
        "in" => "₹",
        "ru" | "by" => "₽",
        "ua" => "₴",
        "kr" => "₩",
        "tr" => "₺",
        "il" => "₪",
        "pl" => "zł",
        "cz" => "Kč",
        "ch" => "CHF",
        "se" | "no" | "dk" | "is" => "kr",
        "vn" => "₫",
        "th" => "฿",
        _ => "€",
    }
}

/// The word list for `language`: a hunspell dictionary under one of
/// `dirs`, preferring the region of `locale` (such as `en_GB.UTF-8`).
pub fn dictionary_path(language: &str, locale: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let locale = locale.split(['.', '@']).next().unwrap_or_default();
    let mut found: Vec<PathBuf> = dirs
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else { return false };
            p.extension().is_some_and(|e| e == "dic") && (stem == language || stem.split_once(['_', '-']).is_some_and(|(l, _)| l == language))
        })
        .collect();
    found.sort();
    let preferred = found.iter().position(|p| p.file_stem().and_then(|s| s.to_str()) == Some(locale));
    match preferred {
        Some(i) => Some(found.swap_remove(i)),
        None => found.into_iter().next(),
    }
}

/// Where hunspell dictionaries are installed.
pub fn dictionary_dirs() -> Vec<PathBuf> {
    ["/usr/share/hunspell", "/usr/share/myspell", "/usr/share/myspell/dicts"].iter().map(PathBuf::from).collect()
}

/// Words to suggest, sorted by their lowercase form.
#[derive(Debug, Default, Clone)]
pub struct Dictionary {
    words: Vec<(String, String)>,
}

impl Dictionary {
    pub fn from_words<I: IntoIterator<Item = String>>(words: I) -> Self {
        let mut words: Vec<(String, String)> = words
            .into_iter()
            .filter(|w| w.chars().count() >= 2 && w.chars().all(|c| c.is_alphabetic() || c == '\'' || c == '-'))
            .map(|w| (w.to_lowercase(), w))
            .collect();
        words.sort();
        words.dedup_by(|a, b| a.0 == b.0);
        Self { words }
    }

    /// A hunspell `.dic` file: a count, then `word/FLAGS` lines.
    pub fn load_hunspell(path: &Path) -> Option<Self> {
        let text = std::fs::read(path).ok()?;
        let text = String::from_utf8_lossy(&text);
        let words = text.lines().skip(1).filter_map(|l| {
            let word = l.split(['/', '\t']).next()?.trim();
            (!word.is_empty()).then(|| word.to_owned())
        });
        Some(Self::from_words(words))
    }

    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Words starting with `prefix` (lowercase).
    fn starting<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        let start = self.words.partition_point(|(lower, _)| lower.as_str() < prefix);
        self.words[start..].iter().take_while(move |(lower, _)| lower.starts_with(prefix)).map(|(_, w)| w.as_str())
    }
}

/// Words typed this session, with how often (kept in memory only).
#[derive(Debug, Default, Clone)]
pub struct Learned(HashMap<String, u32>);

impl Learned {
    pub fn add(&mut self, word: &str) {
        if word.chars().count() >= 2 && word.chars().all(char::is_alphabetic) {
            *self.0.entry(word.to_lowercase()).or_default() += 1;
        }
    }
}

/// Give `word` the case of what was typed: all capitals stay capitals, a
/// first capital stays.
fn match_case(word: &str, typed: &str) -> String {
    let mut typed_chars = typed.chars();
    let first_upper = typed_chars.next().is_some_and(char::is_uppercase);
    if first_upper && typed.chars().count() > 1 && typed.chars().all(|c| !c.is_lowercase()) {
        return word.to_uppercase();
    }
    if first_upper {
        let mut chars = word.chars();
        return chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default();
    }
    word.to_owned()
}

/// Up to `n` completions of `typed`: words typed before (most often
/// first), then the dictionary's, shortest first. The typed text itself
/// is not offered.
pub fn suggest(dictionary: &Dictionary, learned: &Learned, typed: &str, n: usize) -> Vec<String> {
    if typed.is_empty() {
        return Vec::new();
    }
    let prefix = typed.to_lowercase();
    let mut mine: Vec<(&String, &u32)> = learned.0.iter().filter(|(w, _)| w.starts_with(&prefix) && **w != prefix).collect();
    mine.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let mut out: Vec<String> = Vec::new();
    let push = |word: &str, out: &mut Vec<String>| {
        let shown = match_case(word, typed);
        if word.to_lowercase() != prefix && !out.iter().any(|o| o.to_lowercase() == shown.to_lowercase()) {
            out.push(shown);
        }
    };
    for (word, _) in mine {
        if out.len() < n {
            push(word, &mut out);
        }
    }
    if out.len() < n {
        let mut words: Vec<&str> = dictionary.starting(&prefix).collect();
        words.sort_by_key(|w| (w.chars().count(), w.chars().next().is_some_and(char::is_uppercase)));
        for word in words {
            if out.len() >= n {
                break;
            }
            push(word, &mut out);
        }
    }
    out
}

/// The word being typed: the letters right before byte `cursor` of
/// `text`.
pub fn word_before(text: &str, cursor: usize) -> &str {
    let mut cursor = cursor.min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let before = &text[..cursor];
    let start = before.char_indices().rev().take_while(|(_, c)| c.is_alphabetic() || *c == '\'').last().map_or(cursor, |(i, _)| i);
    &before[start..]
}

/// Accented forms offered for a letter (long press), lowercase.
pub fn accents(letter: &str) -> &'static [&'static str] {
    match letter {
        "a" => &["à", "á", "â", "ä", "ã", "å", "ā", "ą", "æ"],
        "c" => &["ç", "ć", "č"],
        "d" => &["ď", "ð"],
        "e" => &["è", "é", "ê", "ë", "ē", "ę", "ě"],
        "g" => &["ğ"],
        "i" => &["ì", "í", "î", "ï", "ī", "ı"],
        "l" => &["ł", "ľ"],
        "n" => &["ñ", "ń", "ň"],
        "o" => &["ò", "ó", "ô", "ö", "õ", "ø", "ō", "ő", "œ"],
        "r" => &["ř"],
        "s" => &["ß", "ś", "š", "ş"],
        "t" => &["ť", "ţ", "þ"],
        "u" => &["ù", "ú", "û", "ü", "ū", "ů", "ű"],
        "y" => &["ý", "ÿ"],
        "z" => &["ź", "ż", "ž"],
        _ => &[],
    }
}

/// The accents for a key's character (capitals for a capital).
pub fn accents_for(ch: &str) -> Vec<String> {
    let lower = ch.to_lowercase();
    let upper = ch != lower;
    accents(&lower)
        .iter()
        .map(|a| if upper && *a != "ß" && *a != "ı" { a.to_uppercase() } else { (*a).to_owned() })
        .collect()
}

/// Whether a text field may get suggestions (`zwp_text_input_v3`
/// content hint and purpose): not for passwords, PINs, numbers,
/// terminals, or fields marked sensitive or hidden.
pub fn suggestions_allowed(hint: u32, purpose: u32) -> bool {
    const HIDDEN_TEXT: u32 = 0x40;
    const SENSITIVE_DATA: u32 = 0x80;
    // digits, number, phone, password, pin, date, time, datetime, terminal
    let refused = [2, 3, 4, 8, 9, 10, 11, 12, 13];
    hint & (HIDDEN_TEXT | SENSITIVE_DATA) == 0 && !refused.contains(&purpose)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(words: &[&str]) -> Dictionary {
        Dictionary::from_words(words.iter().map(|w| w.to_string()))
    }

    #[test]
    fn languages() {
        assert_eq!((language_of_layout("us"), language_of_layout("gb"), language_of_layout("ca")), ("en", "en", "fr"));
        assert_eq!((language_of_layout("de"), language_of_layout("se"), language_of_layout("xx")), ("de", "sv", "en"));
        assert_eq!((currency("us"), currency("gb"), currency("de"), currency("br")), ("$", "£", "€", "R$"));
    }

    #[test]
    fn dictionaries() {
        let dir = std::env::temp_dir().join(format!("helix-osk-dict-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["en_US.dic", "en_GB.dic", "de_DE.dic", "en_US.aff", "fr.dic"] {
            std::fs::write(dir.join(name), "3\nhello/S\nhelp/MS\nhe\n").unwrap();
        }
        let dirs = [dir.clone()];
        assert_eq!(dictionary_path("en", "en_GB.UTF-8", &dirs), Some(dir.join("en_GB.dic")));
        assert_eq!(dictionary_path("en", "de_DE.UTF-8", &dirs), Some(dir.join("en_GB.dic")), "first by name");
        assert_eq!(dictionary_path("fr", "C", &dirs), Some(dir.join("fr.dic")));
        assert_eq!(dictionary_path("it", "it_IT.UTF-8", &dirs), None);
        let d = Dictionary::load_hunspell(&dir.join("en_US.dic")).unwrap();
        assert_eq!(d.len(), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn suggestions() {
        let d = dict(&["the", "there", "these", "theory", "thermometer", "then", "Thebes", "x1", "a"]);
        assert_eq!(d.len(), 7, "single letters and non-words dropped");
        let none = Learned::default();
        assert_eq!(suggest(&d, &none, "the", 3), ["then", "there", "these"], "shortest first, the typed word not offered");
        assert_eq!(suggest(&d, &none, "The", 2), ["Then", "There"], "capital kept");
        assert_eq!(suggest(&d, &none, "THE", 1), ["THEN"]);
        assert_eq!(suggest(&d, &none, "theb", 3), ["Thebes"]);
        assert!(suggest(&d, &none, "", 3).is_empty());
        let mut learned = Learned::default();
        learned.add("thermometer");
        learned.add("thermometer");
        learned.add("theremin");
        assert_eq!(suggest(&d, &learned, "the", 3), ["thermometer", "theremin", "then"], "typed words first");
    }

    #[test]
    fn words_and_accents() {
        assert_eq!(word_before("hello wor", 9), "wor");
        assert_eq!(word_before("hello wor", 5), "hello");
        assert_eq!(word_before("hello ", 6), "");
        assert_eq!(word_before("naïv", 5), "naïv");
        assert_eq!(word_before("don't", 5), "don't");
        assert_eq!(word_before("x", 99), "x");
        assert_eq!(accents_for("e")[1], "é");
        assert_eq!(accents_for("E")[1], "É");
        assert!(accents_for("S").contains(&"ß".to_owned()));
        assert!(accents_for("q").is_empty());
        assert!(suggestions_allowed(0, 0));
        assert!(!suggestions_allowed(0, 8), "password");
        assert!(!suggestions_allowed(0x80, 0), "sensitive");
    }
}
