//! `WHEN` arguments (helixdesktop/spec `contracts/agent-access/cli.md`):
//! an ISO 8601 date or date and time (local time unless it has an
//! offset), or a relative time: `+10m`, `+2h`, `-1d`, `yesterday`,
//! `today`.

use gio::glib;
use helix_i18n::trf;

/// How far ahead `--until` may be (seconds): the shell's own range
/// (helixdesktop/spec `contracts/agent-access/README.md`, "Changes, undo
/// and restoring"; the shell's `agent::changes::UNTIL_MIN` and
/// `UNTIL_MAX`), checked here so a bad one is a usage error before the
/// shell is called.
pub const UNTIL_MIN: i64 = 10;
pub const UNTIL_MAX: i64 = 24 * 60 * 60;

/// Whether `at` is a time `--until` may be at `now`.
pub fn until_in_range(at: i64, now: i64) -> bool {
    (now + UNTIL_MIN..=now + UNTIL_MAX).contains(&at)
}

/// `text` as Unix seconds, relative to `now` (Unix seconds).
pub fn parse(text: &str, now: i64) -> Result<i64, String> {
    let bad = || trf!("“{when}” is not a date, a time or a relative time such as +10m or -1d", when = text);
    let text = text.trim();
    let local = glib::TimeZone::local();
    let midnight = |days_back: i64| -> Result<i64, String> {
        let now = glib::DateTime::from_unix_local(now).map_err(|_| bad())?;
        let day = glib::DateTime::new(&local, now.year(), now.month(), now.day_of_month(), 0, 0, 0.0).map_err(|_| bad())?;
        Ok(day.to_unix() - days_back * 86_400)
    };
    match text {
        "today" => return midnight(0),
        "yesterday" => return midnight(1),
        "now" => return Ok(now),
        _ => {}
    }
    if let Some(sign) = text.chars().next().filter(|c| *c == '+' || *c == '-') {
        let body = &text[1..];
        let split = body.find(|c: char| !c.is_ascii_digit()).unwrap_or(body.len());
        let (n, unit) = body.split_at(split);
        let n: i64 = n.parse().map_err(|_| bad())?;
        let unit = match unit {
            "s" => 1,
            "m" | "min" => 60,
            "h" => 3600,
            "d" => 86_400,
            "w" => 7 * 86_400,
            _ => return Err(bad()),
        };
        let delta = n.checked_mul(unit).ok_or_else(bad)?;
        return Ok(if sign == '+' { now + delta } else { now - delta });
    }
    // A date alone: its midnight, local time.
    if text.len() == 10 && text.as_bytes()[4] == b'-' && text.as_bytes()[7] == b'-' {
        let field = |r: std::ops::Range<usize>| text[r].parse::<i32>().map_err(|_| bad());
        let day = glib::DateTime::new(&local, field(0..4)?, field(5..7)?, field(8..10)?, 0, 0, 0.0).map_err(|_| bad())?;
        return Ok(day.to_unix());
    }
    // A date and time; "2026-11-19 10:00" too.
    let iso = text.replacen(' ', "T", 1);
    let with_seconds = if iso.len() == 16 { format!("{iso}:00") } else { iso };
    glib::DateTime::from_iso8601(&with_seconds, Some(&local)).map(|d| d.to_unix()).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative() {
        let now = 1_800_000_000;
        assert_eq!(parse("+10m", now), Ok(now + 600));
        assert_eq!(parse("-1d", now), Ok(now - 86_400));
        assert_eq!(parse("+2h", now), Ok(now + 7200));
        assert!(parse("+2x", now).is_err());
        assert!(parse("soon", now).is_err());
        let today = parse("today", now).unwrap();
        assert!(today <= now && now - today < 86_400 + 3600);
        assert_eq!(parse("yesterday", now).unwrap(), today - 86_400);
    }

    #[test]
    fn absolute() {
        assert_eq!(parse("2026-11-19T10:00:00Z", 0), Ok(1_795_082_400));
        assert_eq!(parse("2026-11-19T10:00:00+01:00", 0), Ok(1_795_078_800));
        let day = parse("2026-11-19", 0).unwrap();
        assert_eq!(parse("2026-11-19 00:00", 0), Ok(day));
        assert!(parse("2026-13-40", 0).is_err());
    }
}
