//! Translation and locale formats for everything Helix itself shows
//! (helix-desktop-spec `components/i18n.md`).
//!
//! The shell, the portals' dialogs, the session and the greeter share one
//! gettext domain, [`DOMAIN`] (`helix-desktop`): they are released
//! together and share many strings ("Cancel", "Power Off", "Restart"), so
//! translators work on one catalogue (`po/helix-desktop.pot`). Catalogues
//! are installed as `/usr/share/locale/<lang>/LC_MESSAGES/helix-desktop.mo`.
//!
//! Every program calls [`init`] first thing in `main` (the greeter calls
//! [`adopt_system_locale`] before it). Then:
//!
//! - [`tr`]`("Text")` translates a literal and returns a `&'static str`, so
//!   it can stand wherever the literal stood.
//! - [`trn`]`("{n} update", "{n} updates", n)` picks the plural form for
//!   `n` and fills in `{n}`.
//! - [`trc`]`("context", "Text")` disambiguates short strings that need
//!   different translations in different places.
//! - [`trf!`]`("Open {name}", name = file)` translates a message with
//!   placeholders, so translators can move them around; [`trnf!`] is the
//!   same with a plural.
//! - [`n_`]`("Text")` marks a literal in a `const`/`static` table for
//!   extraction without translating it; the code that shows it calls
//!   [`tr`] (or [`tr_dyn`]) on it.
//!
//! `tools/i18n/update-pot.sh` extracts these calls into the template; see
//! `TRANSLATING.md`. The scheme is the same as helix-apps' (its
//! `helix_app_common::i18n`), so both repositories work alike.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use gio::glib;

/// The gettext domain of Helix's desktop components.
pub const DOMAIN: &str = "helix-desktop";

/// Where packages install the compiled catalogues. Overridden at build
/// time by `HELIX_LOCALEDIR` (for other prefixes) and at run time by the
/// `HELIX_LOCALEDIR` environment variable (for tests and uninstalled
/// builds).
pub const LOCALEDIR: &str = match option_env!("HELIX_LOCALEDIR") {
    Some(dir) => dir,
    None => "/usr/share/locale",
};

static READY: AtomicBool = AtomicBool::new(false);
/// 0 = not decided, 1 = left to right, 2 = right to left.
static DIRECTION: AtomicU8 = AtomicU8::new(0);

/// The locale directory in effect: `$HELIX_LOCALEDIR` or [`LOCALEDIR`].
pub fn localedir() -> std::path::PathBuf {
    std::env::var_os("HELIX_LOCALEDIR")
        .filter(|d| !d.is_empty())
        .map(Into::into)
        .unwrap_or_else(|| LOCALEDIR.into())
}

/// Sets the process locale from the environment (`LANGUAGE`, `LC_ALL`,
/// `LC_*`, `LANG`) and binds [`DOMAIN`]. Call it at the start of `main`,
/// before any thread is started and before the first [`tr`]. Calling it
/// again does nothing.
pub fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(bind);
}

fn bind() {
    // SAFETY: called at the start of `main`, before other threads exist,
    // as setlocale(3) requires.
    unsafe {
        gettextrs::setlocale(gettextrs::LocaleCategory::LcAll, "");
    }
    let _ = gettextrs::bindtextdomain(DOMAIN, localedir());
    let _ = gettextrs::bind_textdomain_codeset(DOMAIN, "UTF-8");
    let _ = gettextrs::textdomain(DOMAIN);
    READY.store(true, Ordering::SeqCst);
    DIRECTION.store(0, Ordering::SeqCst);
}

/// Like [`init`], but re-reads the locale environment every time and
/// forgets cached translations. Only for tests and render galleries that
/// switch locales.
#[doc(hidden)]
pub fn init_for_tests() {
    bind();
    cache().lock().unwrap_or_else(|e| e.into_inner()).clear();
    date_cache().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// Whether [`init`] has run (translations are looked up).
pub fn ready() -> bool {
    READY.load(Ordering::Relaxed)
}

fn cache() -> &'static Mutex<HashMap<&'static str, &'static str>> {
    static CACHE: OnceLock<Mutex<HashMap<&'static str, &'static str>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Translates `msgid`.
///
/// Translations are looked up once and kept for the life of the process
/// (the locale never changes while a component runs: a new locale takes
/// effect at the next login), which is what lets this return a
/// `&'static str`. Before [`init`] (unit tests) the message is returned
/// untranslated and not cached.
pub fn tr(msgid: &'static str) -> &'static str {
    if msgid.is_empty() || !ready() {
        return msgid;
    }
    let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(t) = cache.get(msgid) {
        return t;
    }
    let t = gettextrs::dgettext(DOMAIN, msgid);
    let t: &'static str = if t == msgid { msgid } else { Box::leak(t.into_boxed_str()) };
    cache.insert(msgid, t);
    t
}

/// Translates a message that is not a `'static` literal at the call site
/// but was marked for extraction elsewhere (with [`n_`] or [`tr`]).
pub fn tr_dyn(msgid: &str) -> String {
    if msgid.is_empty() || !ready() {
        return msgid.to_string();
    }
    gettextrs::dgettext(DOMAIN, msgid)
}

/// Translates `msgid` in `context` (`msgctxt`), for short strings whose
/// translation depends on where they appear.
pub fn trc(context: &'static str, msgid: &'static str) -> String {
    if !ready() {
        return msgid.to_string();
    }
    // The default domain is DOMAIN (set by init).
    gettextrs::pgettext(context, msgid)
}

/// Picks the translated singular or plural form for `n` and fills in
/// `{n}` (and `{}`) with `n`: `trn("{n} update", "{n} updates", count)`.
pub fn trn(singular: &'static str, plural: &'static str, n: u64) -> String {
    let template = plural_template(singular, plural, n);
    fill(&template, &[&n], &[("n", &n)])
}

/// The translated singular or plural message for `n`, placeholders left
/// in (see [`trnf!`]).
pub fn plural_template(singular: &'static str, plural: &'static str, n: u64) -> String {
    // gettext takes a u32; the plural form of a huge count is the plural
    // form of a huge u32.
    let k = n.min(u32::MAX as u64) as u32;
    if ready() {
        gettextrs::dngettext(DOMAIN, singular, plural, k)
    } else if n == 1 {
        singular.to_string()
    } else {
        plural.to_string()
    }
}

/// Marks `msgid` for extraction without translating it, for `const` and
/// `static` tables; translate it with [`tr`] where it is shown.
pub const fn n_(msgid: &'static str) -> &'static str {
    msgid
}

/// Replaces placeholders in a translated `template`: `{}` takes the next
/// of `positional`, `{0}`, `{1}`… a given one, `{name}` the value named in
/// `named`; `{{` and `}}` are literal braces. Unknown placeholders are
/// kept as they are, so a mistranslated name shows up instead of
/// vanishing.
pub fn fill(template: &str, positional: &[&dyn Display], named: &[(&str, &dyn Display)]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut next = 0;
    let mut rest = template;
    while let Some(i) = rest.find(['{', '}']) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            out.push_str(&tail[..1]);
            rest = &tail[2..];
            continue;
        }
        if let Some(after) = tail.strip_prefix('}') {
            out.push('}');
            rest = after;
            continue;
        }
        let Some(end) = tail.find('}') else {
            out.push_str(tail);
            rest = "";
            break;
        };
        let key = &tail[1..end];
        let value: Option<&dyn Display> = if key.is_empty() {
            let v = positional.get(next).copied();
            next += 1;
            v
        } else if let Ok(k) = key.parse::<usize>() {
            positional.get(k).copied()
        } else {
            named.iter().find(|(name, _)| *name == key).map(|(_, v)| *v)
        };
        match value {
            Some(v) => out.push_str(&v.to_string()),
            None => out.push_str(&tail[..=end]),
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Translates a message with placeholders and fills them in:
///
/// ```
/// # use helix_i18n::trf;
/// let (page, total, file) = (2, 5, "a.txt");
/// assert_eq!(trf!("Page {} of {}", page, total), "Page 2 of 5");
/// assert_eq!(trf!("Open {name}", name = file), "Open a.txt");
/// ```
///
/// The message must be a string literal (it is extracted for
/// translation). Placeholders take no format specs; format the value
/// first (`trf!("{size} free", size = format_size(n))`).
#[macro_export]
macro_rules! trf {
    ($msg:literal $(,)?) => {
        $crate::tr($msg).to_string()
    };
    ($msg:literal, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::fill(
            $crate::tr($msg),
            &[],
            &[$((stringify!($name), &$value as &dyn ::std::fmt::Display)),+],
        )
    };
    ($msg:literal, $($value:expr),+ $(,)?) => {
        $crate::fill(
            $crate::tr($msg),
            &[$(&$value as &dyn ::std::fmt::Display),+],
            &[],
        )
    };
}

/// Like [`trf!`], with singular and plural messages chosen by `n` (which
/// is also available as `{n}`):
///
/// ```
/// # use helix_i18n::trnf;
/// assert_eq!(trnf!("{n} file in {d}", "{n} files in {d}", 2, d = "x"), "2 files in x");
/// ```
#[macro_export]
macro_rules! trnf {
    ($one:literal, $many:literal, $n:expr $(, $name:ident = $value:expr)* $(,)?) => {{
        let n: u64 = ($n) as u64;
        $crate::fill(
            &$crate::plural_template($one, $many, n),
            &[&n as &dyn ::std::fmt::Display],
            &[("n", &n as &dyn ::std::fmt::Display) $(, (stringify!($name), &$value as &dyn ::std::fmt::Display))*],
        )
    }};
}

// ---------------------------------------------------------------------
// Direction

/// Whether the interface reads right to left.
///
/// Decided by the translation, as GTK does: a right-to-left language's
/// catalogue translates the message "left-to-right" (context "text
/// direction") as "right-to-left". `HELIX_TEXT_DIRECTION=rtl|ltr`
/// overrides it (for testing layouts). Untranslated (and before
/// [`init`]) the interface is left to right.
pub fn is_rtl() -> bool {
    match DIRECTION.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let rtl = match std::env::var("HELIX_TEXT_DIRECTION").as_deref() {
                Ok("rtl") => true,
                Ok("ltr") => false,
                // Translators: write "right-to-left" here (in English) if
                // your language is written from right to left, else keep
                // "left-to-right". It is not shown; it mirrors the shell.
                _ => ready() && trc("text direction", "left-to-right") == "right-to-left",
            };
            if ready() {
                DIRECTION.store(if rtl { 2 } else { 1 }, Ordering::Relaxed);
            }
            rtl
        }
    }
}

// ---------------------------------------------------------------------
// Numbers

/// Formats `value` with `decimals` digits after the locale's decimal
/// separator and its digit grouping (`1,234.5` in English, `1.234,5` in
/// German), through the C library's `%'.*f`.
pub fn format_number(value: f64, decimals: usize) -> String {
    let fmt = format!("%'.{decimals}f");
    format_c(&fmt, value).unwrap_or_else(|| format!("{value:.decimals$}"))
}

/// A whole number with the locale's digit grouping.
pub fn format_count(value: u64) -> String {
    format_number(value as f64, 0)
}

/// A percentage as the locale writes it: "45%" in English, "45 %" in
/// French or German, "%45" in Turkish (the translation decides).
pub fn format_percent(percent: f64) -> String {
    // Translators: a percentage; {} is the number. Write it as your
    // language does ("{} %", "%{}", "{}٪"…).
    trf!("{}%", format_number(percent, 0))
}

/// A file or data size in the locale's words and number format ("3.2 MB"),
/// through GLib (decimal units, as GNOME shows sizes).
pub fn format_size(bytes: u64) -> String {
    glib::format_size(bytes).to_string()
}

fn format_c(fmt: &str, value: f64) -> Option<String> {
    extern "C" {
        fn snprintf(buf: *mut std::ffi::c_char, n: usize, fmt: *const std::ffi::c_char, ...) -> i32;
    }
    let cfmt = std::ffi::CString::new(fmt).ok()?;
    let mut buf = vec![0u8; 96];
    // SAFETY: the buffer is writable for its length, the format has one
    // `double` conversion and `value` is passed as a double.
    let n = unsafe { snprintf(buf.as_mut_ptr().cast(), buf.len(), cfmt.as_ptr(), value) };
    if n < 0 || n as usize >= buf.len() {
        return None;
    }
    buf.truncate(n as usize);
    String::from_utf8(buf).ok()
}

// ---------------------------------------------------------------------
// Dates

fn date_cache() -> &'static Mutex<HashMap<(&'static str, u32), &'static str>> {
    static CACHE: OnceLock<Mutex<HashMap<(&'static str, u32), &'static str>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// A date part in the current locale through GLib (`%OB`, `%Ob`, `%A`,
/// `%a`), cached for the life of the process like [`tr`].
fn date_part(format: &'static str, index: u32, date: (i32, i32, i32)) -> &'static str {
    let mut cache = date_cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = cache.get(&(format, index)) {
        return s;
    }
    let text = glib::DateTime::from_utc(date.0, date.1, date.2, 12, 0, 0.0)
        .ok()
        .and_then(|d| d.format(format).ok())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let text: &'static str = Box::leak(text.into_boxed_str());
    cache.insert((format, index), text);
    text
}

/// The month's name on its own, as in a heading ("March"), in the current
/// locale; `month` is 1-12.
pub fn month_name(month: u32) -> &'static str {
    let m = month.clamp(1, 12);
    date_part("%OB", m, (2024, m as i32, 1))
}

/// The month's abbreviated name on its own ("Mar"); `month` is 1-12.
pub fn month_abbr(month: u32) -> &'static str {
    let m = month.clamp(1, 12);
    date_part("%Ob", m, (2024, m as i32, 1))
}

/// The weekday's name ("Monday"); `day` counts from Monday = 0.
pub fn weekday_name(day: u32) -> &'static str {
    let d = day % 7;
    // 1 January 2024 was a Monday.
    date_part("%A", d, (2024, 1, 1 + d as i32))
}

/// The weekday's abbreviated name ("Mon"); `day` counts from Monday = 0.
pub fn weekday_abbr(day: u32) -> &'static str {
    let d = day % 7;
    date_part("%a", d, (2024, 1, 1 + d as i32))
}

/// The first day of the week in the locale (0 = Monday … 6 = Sunday),
/// from the C library's `LC_TIME` (`_NL_TIME_FIRST_WEEKDAY` and
/// `_NL_TIME_WEEK_1STDAY`), as GTK's calendar reads it. Monday without
/// that information.
pub fn first_weekday() -> u32 {
    // glibc's nl_item values (<langinfo.h>, LC_TIME = 2).
    const WEEK_1STDAY: libc::nl_item = 0x20066;
    const FIRST_WEEKDAY: libc::nl_item = 0x20068;
    // SAFETY: nl_langinfo returns a pointer into the locale data; for
    // these items glibc returns the value itself in the pointer (an
    // integer for WEEK_1STDAY) or a pointer to a byte (FIRST_WEEKDAY).
    unsafe {
        let week_1stday = libc::nl_langinfo(WEEK_1STDAY) as usize as u32;
        let first = libc::nl_langinfo(FIRST_WEEKDAY);
        if first.is_null() {
            return 0;
        }
        let first = *first as u8 as u32; // 1 = the day of week_1stday
        // week_1stday is a date: 19971130 was a Sunday, 19971201 a Monday.
        let base = match week_1stday {
            19971130 => 6,
            19971201 => 0,
            _ => return 0,
        };
        if first == 0 {
            return 0;
        }
        (base + first - 1) % 7
    }
}

/// `date` written with a strftime-like GLib `pattern` (from a translated
/// message, so each language puts day, month and weekday in its own
/// order), runs of spaces from padded fields (`%e`, `%l`) made single.
pub fn format_date(date: &glib::DateTime, pattern: &str) -> String {
    date.format(pattern).map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default()
}

// ---------------------------------------------------------------------
// The system locale (for the greeter)

/// The variables that choose a locale, in the order the C library
/// consults them for messages.
const LOCALE_VARS: [&str; 15] = [
    "LANG", "LANGUAGE", "LC_CTYPE", "LC_NUMERIC", "LC_TIME", "LC_COLLATE", "LC_MONETARY", "LC_MESSAGES",
    "LC_PAPER", "LC_NAME", "LC_ADDRESS", "LC_TELEPHONE", "LC_MEASUREMENT", "LC_IDENTIFICATION", "LC_ALL",
];

/// The system-wide locale files, in the order they are read:
/// `/etc/locale.conf` (systemd's, `localectl set-locale`) and
/// `/etc/default/locale` (Debian and Ubuntu's).
pub const SYSTEM_LOCALE_FILES: [&str; 2] = ["/etc/locale.conf", "/etc/default/locale"];

/// The locale assignments in a `locale.conf`-style file (`KEY=value`
/// lines, optionally quoted, `#` comments).
pub fn parse_locale_conf(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with('#') {
                return None;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            if !LOCALE_VARS.contains(&key) {
                return None;
            }
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
            (!value.is_empty()).then(|| (key.to_owned(), value.to_owned()))
        })
        .collect()
}

/// For the greeter, which runs before anyone logs in: when the
/// environment names no locale (greetd does not always pass one), adopt
/// the system's from [`SYSTEM_LOCALE_FILES`]. Returns the assignments it
/// made. Call it before [`init`] and before any thread starts.
pub fn adopt_system_locale() -> Vec<(String, String)> {
    adopt_from(&SYSTEM_LOCALE_FILES.map(std::path::Path::new))
}

fn adopt_from(files: &[&std::path::Path]) -> Vec<(String, String)> {
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    if set("LANG") || set("LC_ALL") || set("LC_MESSAGES") {
        return Vec::new();
    }
    let Some(found) = files.iter().find_map(|f| std::fs::read_to_string(f).ok().map(|t| parse_locale_conf(&t)).filter(|v| !v.is_empty())) else {
        return Vec::new();
    };
    for (k, v) in &found {
        if !set(k) {
            std::env::set_var(k, v);
        }
    }
    found
}

/// The locale messages are shown in (`LC_ALL`, `LC_MESSAGES` or `LANG`,
/// the first that is set), e.g. `de_DE.UTF-8`; `C` without one.
pub fn messages_locale() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "C".to_owned())
}

/// The suffixes to try for a localized desktop-entry key (`Name[xx]`), best
/// first, for `locale` (`lang_COUNTRY.ENCODING@MODIFIER`), as the Desktop
/// Entry Specification orders them: `lang_COUNTRY@MODIFIER`,
/// `lang_COUNTRY`, `lang@MODIFIER`, `lang`. Empty for `C` and `POSIX`.
pub fn locale_key_candidates(locale: &str) -> Vec<String> {
    let (rest, modifier) = match locale.split_once('@') {
        Some((r, m)) => (r, Some(m)),
        None => (locale, None),
    };
    let rest = rest.split('.').next().unwrap_or(rest);
    let (lang, country) = match rest.split_once('_') {
        Some((l, c)) => (l, Some(c)),
        None => (rest, None),
    };
    if lang.is_empty() || lang == "C" || lang == "POSIX" {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let (Some(c), Some(m)) = (country, modifier) {
        out.push(format!("{lang}_{c}@{m}"));
    }
    if let Some(c) = country {
        out.push(format!("{lang}_{c}"));
    }
    if let Some(m) = modifier {
        out.push(format!("{lang}@{m}"));
    }
    out.push(lang.to_owned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_placeholders() {
        assert_eq!(fill("Page {} of {}", &[&2, &5], &[]), "Page 2 of 5");
        assert_eq!(fill("{1} before {0}", &[&"a", &"b"], &[]), "b before a");
        assert_eq!(fill("Open {name}", &[], &[("name", &"x.txt")]), "Open x.txt");
        assert_eq!(fill("{{literal}} {missing}", &[], &[]), "{literal} {missing}");
        assert_eq!(fill("unbalanced {", &[], &[]), "unbalanced {");
        assert_eq!(fill("ünï {} ✓", &[&1], &[]), "ünï 1 ✓");
    }

    #[test]
    fn untranslated_before_init() {
        assert_eq!(tr("Power Off"), "Power Off");
        assert_eq!(trn("{n} update", "{n} updates", 1), "1 update");
        assert_eq!(trn("{n} update", "{n} updates", 3), "3 updates");
        assert_eq!(trf!("Open {name}", name = "a"), "Open a");
        assert_eq!(trf!("{} of {}", 1, 2), "1 of 2");
        assert_eq!(trnf!("{n} file in {d}", "{n} files in {d}", 2, d = "x"), "2 files in x");
        assert_eq!(n_("Marked"), "Marked");
        assert_eq!(trc("ctx", "Open"), "Open");
        assert!(!is_rtl());
    }

    #[test]
    fn c_locale_formats() {
        assert_eq!(month_name(1), "January");
        assert_eq!(month_abbr(12), "Dec");
        assert_eq!(weekday_name(0), "Monday");
        assert_eq!(weekday_abbr(6), "Sun");
        assert_eq!(format_number(1234.5, 1), "1234.5");
        assert_eq!(format_percent(45.0), "45%");
        assert_eq!(format_size(3_200_000), "3.2\u{a0}MB");
        let d = glib::DateTime::from_utc(2026, 11, 12, 14, 5, 0.0).unwrap();
        assert_eq!(format_date(&d, "%A, %B %e"), "Thursday, November 12");
    }

    #[test]
    fn locale_conf() {
        let text = "# system locale\nLANG=de_DE.UTF-8\nLC_TIME=\"en_GB.UTF-8\"\nexport LC_MESSAGES='fr_FR.UTF-8'\nFOO=bar\nLANGUAGE=\n";
        assert_eq!(
            parse_locale_conf(text),
            [("LANG".into(), "de_DE.UTF-8".into()), ("LC_TIME".into(), "en_GB.UTF-8".into()), ("LC_MESSAGES".into(), "fr_FR.UTF-8".into())]
        );
    }

    #[test]
    fn desktop_key_candidates() {
        assert_eq!(locale_key_candidates("sr_RS.UTF-8@latin"), ["sr_RS@latin", "sr_RS", "sr@latin", "sr"]);
        assert_eq!(locale_key_candidates("de_DE.UTF-8"), ["de_DE", "de"]);
        assert_eq!(locale_key_candidates("he"), ["he"]);
        assert!(locale_key_candidates("C.UTF-8").is_empty());
        assert!(locale_key_candidates("POSIX").is_empty());
    }
}
