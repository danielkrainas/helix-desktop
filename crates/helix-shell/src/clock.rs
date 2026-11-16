//! How the shell writes times (helixdesktop/spec
//! `contracts/gsettings/calendar.md`, "Clock"): the top bar's clock and
//! the times in the message tray and on the lock screen follow
//! `org.gnome.desktop.interface` `clock-format`, `clock-show-date`,
//! `clock-show-weekday` and `clock-show-seconds`.

use std::sync::RwLock;

use gio::glib::DateTime;
use helix_i18n::{trc, trf};

pub const SCHEMA: &str = "org.gnome.desktop.interface";

/// The clock settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockFormat {
    pub twelve_hour: bool,
    pub date: bool,
    pub weekday: bool,
    pub seconds: bool,
}

impl Default for ClockFormat {
    /// The schema's defaults.
    fn default() -> Self {
        Self { twelve_hour: false, date: true, weekday: false, seconds: false }
    }
}

static CURRENT: RwLock<ClockFormat> = RwLock::new(ClockFormat { twelve_hour: false, date: true, weekday: false, seconds: false });

/// The settings as they are now (the schema's defaults without it).
pub fn read() -> ClockFormat {
    if !helix_settings::has_schema(SCHEMA) {
        return ClockFormat::default();
    }
    let s = helix_settings::Settings::new(SCHEMA);
    let has = |k: &str| s.settings_schema().is_some_and(|schema| schema.has_key(k));
    let flag = |k: &str, default: bool| if has(k) { s.boolean(k) } else { default };
    ClockFormat {
        twelve_hour: has("clock-format") && s.string("clock-format") == "12h",
        date: flag("clock-show-date", true),
        weekday: flag("clock-show-weekday", false),
        seconds: flag("clock-show-seconds", false),
    }
}

/// The format in use.
pub fn current() -> ClockFormat {
    *CURRENT.read().unwrap_or_else(|e| e.into_inner())
}

/// Follows the settings for the rest of the process.
pub async fn follow() {
    if !helix_settings::has_schema(SCHEMA) {
        return;
    }
    let watched = helix_settings::GSettings::new(&[SCHEMA]);
    let (mut changes, _watch) = watched.watch();
    loop {
        *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = read();
        if changes.recv().await.is_none() {
            break;
        }
    }
}

/// `d` formatted with a strftime-like `pattern`, runs of spaces (from
/// padded fields such as `%e`) made single.
fn format(d: &DateTime, pattern: &str) -> String {
    d.format(pattern).map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default()
}

/// The time of day: "14:05" or "2:05 PM", with seconds when asked. The
/// patterns are translatable (GLib `DateTime::format`), so a language
/// can write its own AM/PM order or separators.
pub fn time_pattern(f: ClockFormat, seconds: bool) -> String {
    match (f.twelve_hour, seconds) {
        // Translators: a time in 24-hour format (GLib DateTime format).
        (false, false) => trc("24-hour time", "%H:%M"),
        // Translators: a time in 24-hour format with seconds.
        (false, true) => trc("24-hour time with seconds", "%H:%M:%S"),
        // Translators: a time in 12-hour format; %l is the hour without
        // padding, %p AM or PM in the locale's words.
        (true, false) => trc("12-hour time", "%l:%M %p"),
        // Translators: a time in 12-hour format with seconds.
        (true, true) => trc("12-hour time with seconds", "%l:%M:%S %p"),
    }
}

/// A time of day as the user wants times written (no seconds).
pub fn time_of_day(f: ClockFormat, d: &DateTime) -> String {
    format(d, &time_pattern(f, false))
}

/// The top bar's clock: [weekday] [month day] time, e.g.
/// "Thu Nov 12 2:05 PM" (the date part is translatable, so each language
/// orders day and month its own way).
pub fn top_bar(f: ClockFormat, d: &DateTime) -> String {
    let time = format(d, &time_pattern(f, f.seconds));
    let date = match (f.weekday, f.date) {
        // Translators: the top bar clock's date with the weekday (GLib
        // DateTime format): "Thu Nov 12".
        (true, true) => trc("top bar date with weekday", "%a %b %e"),
        // Translators: the top bar clock's date: "Nov 12".
        (false, true) => trc("top bar date", "%b %e"),
        // Translators: the top bar clock's weekday: "Thu".
        (true, false) => trc("top bar weekday", "%a"),
        (false, false) => return time,
    };
    // Translators: the top bar clock: a date part and the time.
    trf!("{date} {time}", date = format(d, &date), time = time)
}

/// The lock screen's big clock: the time only, without seconds.
pub fn lock_screen(f: ClockFormat, d: &DateTime) -> String {
    time_of_day(f, d)
}

/// The lock screen's clock in two parts: the digits, and in 12-hour
/// format the AM/PM suffix (drawn smaller).
pub fn lock_screen_parts(f: ClockFormat, d: &DateTime) -> (String, Option<String>) {
    if f.twelve_hour {
        // Translators: the lock screen's clock digits in 12-hour format
        // (the AM/PM suffix is drawn apart, with %p).
        (format(d, &trc("lock screen 12-hour time", "%l:%M")), Some(format(d, "%p")).filter(|s| !s.is_empty()))
    } else {
        (format(d, &trc("24-hour time", "%H:%M")), None)
    }
}

/// The lock screen's date line, e.g. "Thursday, November 12" (names in
/// the locale's language, order from the translation).
pub fn lock_date(d: &DateTime) -> String {
    // Translators: the lock screen's date (GLib DateTime format):
    // "Thursday, November 12".
    format(d, &trc("lock screen date", "%A, %B %e"))
}

/// Now, in local time.
pub fn now() -> Option<DateTime> {
    DateTime::now_local().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gio::glib::TimeZone;

    fn at(h: i32, m: i32, s: f64) -> DateTime {
        // A Thursday.
        DateTime::new(&TimeZone::utc(), 2026, 11, 12, h, m, s).unwrap()
    }

    #[test]
    fn formats() {
        // Month and weekday names follow the locale; tests run in C.
        let d = at(14, 5, 9.0);
        let f = ClockFormat::default();
        assert_eq!(time_of_day(f, &d), "14:05");
        assert_eq!(top_bar(f, &d), "Nov 12 14:05");
        assert_eq!(top_bar(ClockFormat { date: false, ..f }, &d), "14:05");
        assert_eq!(top_bar(ClockFormat { weekday: true, ..f }, &d), "Thu Nov 12 14:05");
        assert_eq!(top_bar(ClockFormat { weekday: true, date: false, seconds: true, ..f }, &d), "Thu 14:05:09");
        let twelve = ClockFormat { twelve_hour: true, date: false, ..f };
        assert_eq!(top_bar(twelve, &d), "2:05 PM");
        assert_eq!(top_bar(ClockFormat { seconds: true, ..twelve }, &at(0, 30, 0.0)), "12:30:00 AM");
        assert_eq!(lock_screen(ClockFormat { seconds: true, ..twelve }, &d), "2:05 PM");
        assert_eq!(lock_screen_parts(twelve, &d), ("2:05".to_owned(), Some("PM".to_owned())));
        assert_eq!(lock_screen_parts(f, &d), ("14:05".to_owned(), None));
        assert_eq!(lock_date(&d), "Thursday, November 12");
    }
}
