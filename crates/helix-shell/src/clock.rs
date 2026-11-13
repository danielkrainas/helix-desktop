//! How the shell writes times (helix-desktop-spec
//! `contracts/gsettings/calendar.md`, "Clock"): the top bar's clock and
//! the times in the message tray and on the lock screen follow
//! `org.gnome.desktop.interface` `clock-format`, `clock-show-date`,
//! `clock-show-weekday` and `clock-show-seconds`.

use std::sync::RwLock;

use gio::glib::DateTime;
use gio::prelude::*;

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
    let s = gio::Settings::new(SCHEMA);
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

/// The time of day: "14:05" or "2:05 PM", with seconds when asked.
pub fn time_pattern(f: ClockFormat, seconds: bool) -> &'static str {
    match (f.twelve_hour, seconds) {
        (false, false) => "%H:%M",
        (false, true) => "%H:%M:%S",
        (true, false) => "%l:%M %p",
        (true, true) => "%l:%M:%S %p",
    }
}

/// A time of day as the user wants times written (no seconds).
pub fn time_of_day(f: ClockFormat, d: &DateTime) -> String {
    format(d, time_pattern(f, false))
}

/// The top bar's clock: [weekday] [month day] time, e.g.
/// "Thu Nov 12 2:05 PM".
pub fn top_bar(f: ClockFormat, d: &DateTime) -> String {
    let mut pattern = String::new();
    if f.weekday {
        pattern.push_str("%a ");
    }
    if f.date {
        pattern.push_str("%b %e ");
    }
    pattern.push_str(time_pattern(f, f.seconds));
    format(d, &pattern)
}

/// The lock screen's big clock: the time only, without seconds.
pub fn lock_screen(f: ClockFormat, d: &DateTime) -> String {
    time_of_day(f, d)
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
    }
}
