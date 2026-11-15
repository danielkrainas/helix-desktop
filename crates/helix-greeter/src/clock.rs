//! The clock: time and date in the locale's words (C library `strftime`),
//! in the 12- or 24-hour format chosen at start (see [`decide`]).

use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};

use helix_i18n::tr;

static TWELVE_HOUR: AtomicBool = AtomicBool::new(false);

/// Where the 12/24-hour choice came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `clock-format` in greeter.toml.
    Config,
    /// `org.gnome.desktop.interface clock-format` as the greeter user
    /// reads it (the system's defaults and dconf overrides).
    Settings,
    /// The system locale's time format.
    Locale,
}

/// Picks 12- or 24-hour time (helix-desktop-spec `components/greeter.md`,
/// "Clock"): `configured` (greeter.toml's `clock-format`), else the
/// `clock-format` setting `settings` read, else the locale's own time
/// format. Remembered for [`time`].
pub fn decide(configured: Option<&str>, settings: Option<String>) -> (bool, Source) {
    let parse = |v: &str| match v.trim() {
        "12h" => Some(true),
        "24h" => Some(false),
        _ => None,
    };
    let (twelve, source) = if let Some(t) = configured.and_then(parse) {
        (t, Source::Config)
    } else if let Some(t) = settings.as_deref().and_then(parse) {
        (t, Source::Settings)
    } else {
        (locale_twelve_hour(), Source::Locale)
    };
    TWELVE_HOUR.store(twelve, Ordering::Relaxed);
    (twelve, source)
}

/// `org.gnome.desktop.interface clock-format`, when the schema is
/// installed.
pub fn settings_clock_format() -> Option<String> {
    const SCHEMA: &str = "org.gnome.desktop.interface";
    let schema = gio::SettingsSchemaSource::default()?.lookup(SCHEMA, true)?;
    if !schema.has_key("clock-format") {
        return None;
    }
    use gio::prelude::*;
    Some(gio::Settings::new(SCHEMA).string("clock-format").to_string())
}

/// Whether the locale writes times with AM/PM (its `T_FMT` uses a
/// 12-hour field), as `en_US` does and `de_DE` does not.
pub fn locale_twelve_hour() -> bool {
    // SAFETY: nl_langinfo returns a NUL-terminated string owned by the C
    // library, copied at once.
    let fmt = unsafe {
        let p = libc::nl_langinfo(libc::T_FMT);
        if p.is_null() {
            return false;
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    };
    ["%r", "%I", "%l", "%p"].iter().any(|f| fmt.contains(f))
}

/// `now` (seconds since the epoch) as local time, formatted by `pattern`.
pub fn format(now: i64, pattern: &str) -> String {
    let Ok(pattern) = CString::new(pattern) else {
        return String::new();
    };
    let t = now as libc::time_t;
    // SAFETY: localtime_r fills `tm`; strftime writes at most `buf.len()`
    // bytes and returns how many.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
        let mut buf = [0u8; 128];
        let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), pattern.as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// "14:05", or "2:05 PM" in 12-hour time.
pub fn time(now: i64) -> String {
    let pattern = if TWELVE_HOUR.load(Ordering::Relaxed) {
        // Translators: the login screen's clock in 12-hour time (strftime).
        tr("%l:%M %p")
    } else {
        // Translators: the login screen's clock in 24-hour time (strftime).
        tr("%H:%M")
    };
    format(now, pattern).trim().to_owned()
}

/// "Thursday, November 12".
pub fn date(now: i64) -> String {
    // Translators: the login screen's date (strftime): weekday, month and
    // day, in your language's order.
    format(now, tr("%A, %B %-d"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (One test: the 12/24-hour choice is global.)
    #[test]
    fn formats_and_clock_format() {
        let t = time(0);
        assert_eq!(t.len(), 5, "{t}");
        assert_eq!(&t[2..3], ":");
        assert!(!date(0).is_empty());
        // The epoch in any time zone.
        assert!(["1969", "1970"].contains(&format(0, "%Y").as_str()));
        assert_eq!(format(0, "bad\0"), "");

        assert_eq!(decide(Some("12h"), Some("24h".into())), (true, Source::Config));
        assert_eq!(decide(Some("bogus"), Some("12h".into())), (true, Source::Settings));
        assert_eq!(decide(None, Some("24h".into())), (false, Source::Settings));
        // Tests run in the C locale, whose time format is 24-hour.
        assert_eq!(decide(None, None), (false, Source::Locale));
        let twelve = { decide(Some("12h"), None); time(13 * 3600 + 5 * 60) };
        assert!(twelve.ends_with("PM") || twelve.ends_with("AM"), "{twelve}");
        decide(Some("24h"), None);
    }
}
