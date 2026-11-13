//! Just enough iCalendar (RFC 5545) for the message tray's events
//! (helix-desktop-spec `contracts/gsettings/calendar.md`, "Events"):
//! events, their times, and the recurrences Helix expands.

use std::collections::HashSet;

use gio::glib::{DateTime, TimeZone};

/// One occurrence of an event, in Unix seconds (end exclusive). All-day
/// occurrences run from local midnight to local midnight.
#[derive(Debug, Clone, PartialEq)]
pub struct Occurrence {
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    pub summary: String,
}

impl Occurrence {
    /// Whether it overlaps `[from, to)`; an instant counts at its start.
    pub fn overlaps(&self, from: i64, to: i64) -> bool {
        self.start < to && self.end.max(self.start + 1) > from
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Zone {
    Utc,
    Named(String),
    Floating,
}

/// A DATE or DATE-TIME value.
#[derive(Debug, Clone, PartialEq)]
struct Time {
    date: (i32, i32, i32),
    time: Option<(i32, i32, i32)>,
    zone: Zone,
}

impl Time {
    fn tz(&self) -> TimeZone {
        match &self.zone {
            Zone::Utc => TimeZone::utc(),
            Zone::Named(id) => zone(olson(id)).unwrap_or_else(TimeZone::local),
            Zone::Floating => TimeZone::local(),
        }
    }

    /// As a date-time (all-day: local midnight).
    fn datetime(&self) -> Option<DateTime> {
        let (y, m, d) = self.date;
        let tz = if self.time.is_none() { TimeZone::local() } else { self.tz() };
        let (h, mi, s) = self.time.unwrap_or((0, 0, 0));
        DateTime::new(&tz, y, m, d, h, mi, f64::from(s)).ok()
    }
}

/// The time zone named `id`, if the system knows it.
pub fn zone(id: &str) -> Option<TimeZone> {
    TimeZone::from_identifier(Some(id))
}

/// An Olson name from a TZID that may carry a prefix
/// (`/freeassociation.sourceforge.net/Europe/Berlin`).
fn olson(tzid: &str) -> &str {
    let id = tzid.rsplit_once("sourceforge.net/").map_or(tzid, |(_, rest)| rest);
    let id = id.strip_prefix("Tzfile/").unwrap_or(id);
    id.trim_start_matches('/')
}

#[derive(Debug, Clone, Default)]
struct Event {
    uid: String,
    summary: String,
    start: Option<Time>,
    end: Option<Time>,
    duration: Option<i64>,
    rrule: Option<String>,
    rdates: Vec<Time>,
    exdates: Vec<Time>,
    /// `EXRULE`s (RFC 2445): their occurrences are left out.
    exrules: Vec<String>,
    recurrence_id: Option<Time>,
    cancelled: bool,
}

/// Content lines, unfolded.
fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)) {
        match (line.strip_prefix(' ').or_else(|| line.strip_prefix('\t')), out.last_mut()) {
            (Some(rest), Some(last)) => last.push_str(rest),
            _ => out.push(line.to_owned()),
        }
    }
    out
}

/// A property line's name, parameters and value.
type Prop = (String, Vec<(String, String)>, String);

/// `NAME;PARAM=V:value` → (name, params, value).
fn split_prop(line: &str) -> Option<Prop> {
    // The value starts at the first ':' outside quotes.
    let mut quoted = false;
    let colon = line.char_indices().find(|&(_, c)| {
        if c == '"' {
            quoted = !quoted;
        }
        c == ':' && !quoted
    })?.0;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let mut parts = head.split(';');
    let name = parts.next()?.to_ascii_uppercase();
    let params = parts
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.to_ascii_uppercase(), v.trim_matches('"').to_owned()))
        .collect();
    Some((name, params, value.to_owned()))
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n' | 'N') => out.push(' '),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_time(params: &[(String, String)], value: &str) -> Option<Time> {
    let value = value.trim();
    let num = |s: &str| s.parse::<i32>().ok();
    let date = (num(value.get(0..4)?)?, num(value.get(4..6)?)?, num(value.get(6..8)?)?);
    let time = match value.get(8..9) {
        Some("T") => Some((num(value.get(9..11)?)?, num(value.get(11..13)?)?, num(value.get(13..15)?)?)),
        _ => None,
    };
    let zone = if value.ends_with('Z') {
        Zone::Utc
    } else if let Some((_, id)) = params.iter().find(|(k, _)| k == "TZID") {
        Zone::Named(id.clone())
    } else {
        Zone::Floating
    };
    Some(Time { date, time, zone })
}

/// An RFC 5545 duration in seconds (`P1D`, `-PT15M`, `P2W`).
fn parse_duration(s: &str) -> Option<i64> {
    let (sign, rest) = match s.strip_prefix('-') {
        Some(r) => (-1, r),
        None => (1, s.strip_prefix('+').unwrap_or(s)),
    };
    let rest = rest.strip_prefix('P')?;
    let (mut total, mut n, mut in_time) = (0i64, String::new(), false);
    for c in rest.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' => n.push(c),
            _ => {
                let v: i64 = n.parse().ok()?;
                n.clear();
                total += v * match (c, in_time) {
                    ('W', _) => 604_800,
                    ('D', _) => 86_400,
                    ('H', true) => 3600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => return None,
                };
            }
        }
    }
    Some(sign * total)
}

fn parse_events(text: &str) -> Vec<Event> {
    let mut out = Vec::new();
    let mut current: Option<Event> = None;
    // Nested components (alarms) inside an event are skipped.
    let mut depth = 0;
    for line in unfold(text) {
        let Some((name, params, value)) = split_prop(&line) else { continue };
        match (name.as_str(), value.to_ascii_uppercase().as_str()) {
            ("BEGIN", "VEVENT") if current.is_none() => current = Some(Event::default()),
            ("BEGIN", _) if current.is_some() => depth += 1,
            ("END", "VEVENT") if depth == 0 => out.extend(current.take()),
            ("END", _) if depth > 0 => depth -= 1,
            _ => {
                let Some(e) = current.as_mut().filter(|_| depth == 0) else { continue };
                match name.as_str() {
                    "UID" => e.uid = value,
                    "SUMMARY" => e.summary = unescape(&value),
                    "DTSTART" => e.start = parse_time(&params, &value),
                    "DTEND" => e.end = parse_time(&params, &value),
                    "DURATION" => e.duration = parse_duration(&value),
                    "RRULE" => e.rrule = Some(value),
                    "EXRULE" => e.exrules.push(value),
                    "EXDATE" => e.exdates.extend(value.split(',').filter_map(|v| parse_time(&params, v))),
                    // A PERIOD value counts by its start.
                    "RDATE" => e.rdates.extend(value.split(',').filter_map(|v| parse_time(&params, v.split('/').next().unwrap_or(v)))),
                    "RECURRENCE-ID" => e.recurrence_id = parse_time(&params, &value),
                    "STATUS" => e.cancelled = value.eq_ignore_ascii_case("CANCELLED"),
                    _ => {}
                }
            }
        }
    }
    out
}

/// A weekday in a `BYDAY` list: 1 = Monday … 7 = Sunday, with an
/// optional position (`2TU`: the second Tuesday; `-1FR`: the last Friday).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WeekdayNum {
    nth: i32,
    day: i32,
}

#[derive(Debug, Default, PartialEq)]
struct Rule {
    freq: String,
    interval: i32,
    count: Option<u32>,
    until: Option<i64>,
    by_day: Vec<WeekdayNum>,
    by_month: Vec<i32>,
    by_month_day: Vec<i32>,
    by_year_day: Vec<i32>,
    by_set_pos: Vec<i32>,
    by_week_no: Vec<i32>,
    by_hour: Vec<i32>,
    by_minute: Vec<i32>,
    by_second: Vec<i32>,
    /// The week's first day (1 = Monday … 7 = Sunday).
    week_start: i32,
}

const DAYS: [&str; 7] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];

fn weekday_of(name: &str) -> Option<i32> {
    DAYS.iter().position(|d| d.eq_ignore_ascii_case(name)).map(|i| i as i32 + 1)
}

fn numbers(v: &str, valid: impl Fn(i32) -> bool) -> Vec<i32> {
    v.split(',').filter_map(|n| n.trim().trim_start_matches('+').parse().ok()).filter(|n| valid(*n)).collect()
}

fn parse_rule(s: &str) -> Rule {
    let mut rule = Rule { interval: 1, week_start: 1, ..Rule::default() };
    for part in s.split(';') {
        let Some((k, v)) = part.split_once('=') else { continue };
        match k.to_ascii_uppercase().as_str() {
            "FREQ" => rule.freq = v.to_ascii_uppercase(),
            "INTERVAL" => rule.interval = v.parse().unwrap_or(1).max(1),
            "COUNT" => rule.count = v.parse().ok(),
            "UNTIL" => rule.until = parse_time(&[], v).and_then(|t| t.datetime()).map(|d| d.to_unix()),
            "BYDAY" => {
                rule.by_day = v
                    .split(',')
                    .filter_map(|d| {
                        let d = d.trim();
                        let split = d.len().checked_sub(2)?;
                        let day = weekday_of(d.get(split..)?)?;
                        let nth = match d.get(..split)?.trim_start_matches('+') {
                            "" => 0,
                            n => n.parse().ok().filter(|n: &i32| (1..=53).contains(&n.abs()))?,
                        };
                        Some(WeekdayNum { nth, day })
                    })
                    .collect();
            }
            "BYMONTH" => rule.by_month = numbers(v, |n| (1..=12).contains(&n)),
            "BYMONTHDAY" => rule.by_month_day = numbers(v, |n| (1..=31).contains(&n.abs())),
            "BYYEARDAY" => rule.by_year_day = numbers(v, |n| (1..=366).contains(&n.abs())),
            "BYSETPOS" => rule.by_set_pos = numbers(v, |n| (1..=366).contains(&n.abs())),
            "WKST" => rule.week_start = weekday_of(v).unwrap_or(1),
            "BYWEEKNO" => rule.by_week_no = numbers(v, |n| (1..=53).contains(&n.abs())),
            "BYHOUR" => rule.by_hour = numbers(v, |n| (0..=23).contains(&n)),
            "BYMINUTE" => rule.by_minute = numbers(v, |n| (0..=59).contains(&n)),
            "BYSECOND" => rule.by_second = numbers(v, |n| (0..=60).contains(&n)),
            _ => {}
        }
    }
    rule
}

// ----- Plain calendar dates ------------------------------------------------

type Date = (i32, i32, i32);

fn leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i32, m: i32) -> i32 {
    match m {
        2 if leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_in_year(y: i32) -> i32 {
    if leap(y) { 366 } else { 365 }
}

/// Days since 1970-01-01 (proleptic Gregorian).
fn day_number((y, m, d): Date) -> i64 {
    let y = i64::from(if m <= 2 { y - 1 } else { y });
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn from_day_number(n: i64) -> Date {
    let z = n + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as i32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as i32;
    let y = (yoe + era * 400) as i32 + i32::from(m <= 2);
    (y, m, d)
}

/// 1 = Monday … 7 = Sunday.
fn weekday(date: Date) -> i32 {
    // 1970-01-01 was a Thursday.
    ((day_number(date) + 3).rem_euclid(7) + 1) as i32
}

fn year_day((y, m, d): Date) -> i32 {
    (day_number((y, m, d)) - day_number((y, 1, 1))) as i32 + 1
}

/// The days of `days` (inclusive range of day numbers) that are `wd`.
fn weekdays_in(first: i64, last: i64, wd: WeekdayNum) -> Vec<i64> {
    let offset = (i64::from(wd.day) - i64::from(weekday(from_day_number(first)))).rem_euclid(7);
    let all: Vec<i64> = (first + offset..=last).step_by(7).collect();
    match wd.nth {
        0 => all,
        n if n > 0 => all.get(n as usize - 1).copied().into_iter().collect(),
        n => all.len().checked_sub(n.unsigned_abs() as usize).and_then(|i| all.get(i)).copied().into_iter().collect(),
    }
}

/// A month day (negative: from the end), if the month has it.
fn month_day(y: i32, m: i32, n: i32) -> Option<Date> {
    let len = days_in_month(y, m);
    let d = if n > 0 { n } else { len + n + 1 };
    (1..=len).contains(&d).then_some((y, m, d))
}

/// Whether `date` passes the rule parts that only limit (for frequencies
/// where they do not expand).
fn passes(rule: &Rule, date: Date, check_month_day: bool, check_day: bool) -> bool {
    let (y, m, d) = date;
    (rule.by_month.is_empty() || rule.by_month.contains(&m))
        && (!check_month_day || rule.by_month_day.is_empty() || rule.by_month_day.iter().any(|&n| month_day(y, m, n) == Some(date)))
        && (rule.by_year_day.is_empty()
            || rule.by_year_day.iter().any(|&n| {
                let len = days_in_year(y);
                let k = if n > 0 { n } else { len + n + 1 };
                year_day(date) == k
            }))
        && (!check_day || rule.by_day.is_empty() || rule.by_day.iter().any(|w| w.day == weekday(date)))
        && d > 0
}

/// The dates of one month, as MONTHLY (and YEARLY with months) expand.
fn month_dates(rule: &Rule, y: i32, m: i32, start: Date) -> Vec<i64> {
    let first = day_number((y, m, 1));
    let last = first + i64::from(days_in_month(y, m)) - 1;
    let mut days: Vec<i64> = if !rule.by_month_day.is_empty() {
        let mut days: Vec<i64> = rule.by_month_day.iter().filter_map(|&n| month_day(y, m, n)).map(day_number).collect();
        if !rule.by_day.is_empty() {
            // BYDAY limits: plain weekdays, or numbered ones in the month.
            days.retain(|&n| rule.by_day.iter().any(|&w| weekdays_in(first, last, w).contains(&n)));
        }
        days
    } else if !rule.by_day.is_empty() {
        rule.by_day.iter().flat_map(|&w| weekdays_in(first, last, w)).collect()
    } else {
        month_day(y, m, start.2).map(day_number).into_iter().collect()
    };
    days.retain(|&n| passes(rule, from_day_number(n), false, false));
    days
}

/// The first day of week 1 of year `y`, weeks starting on `week_start`:
/// the first week with at least four days in the year.
fn week_one(y: i32, week_start: i32) -> i64 {
    let jan1 = day_number((y, 1, 1));
    let offset = i64::from((weekday((y, 1, 1)) - week_start).rem_euclid(7));
    if 7 - offset >= 4 { jan1 - offset } else { jan1 - offset + 7 }
}

/// The days of the weeks `BYWEEKNO` names in year `y`: the `BYDAY`
/// weekdays (else the start's weekday) of each.
fn week_number_days(rule: &Rule, y: i32, start: Date) -> Vec<i64> {
    let first = week_one(y, rule.week_start);
    let weeks = (week_one(y + 1, rule.week_start) - first) / 7;
    let wanted: Vec<i32> = if rule.by_day.is_empty() { vec![weekday(start)] } else { rule.by_day.iter().map(|w| w.day).collect() };
    rule.by_week_no
        .iter()
        .filter_map(|&n| {
            let n = if n > 0 { i64::from(n) } else { weeks + i64::from(n) + 1 };
            (1..=weeks).contains(&n).then(|| first + 7 * (n - 1))
        })
        .flat_map(|week| (week..week + 7).filter(|&d| wanted.contains(&weekday(from_day_number(d)))).collect::<Vec<_>>())
        .filter(|&d| passes(rule, from_day_number(d), true, false))
        .collect()
}

/// The candidate dates (day numbers) of the period `k` periods after the
/// start's.
fn period_dates(rule: &Rule, start: Date, k: i64) -> Vec<i64> {
    let step = k * i64::from(rule.interval);
    let mut days: Vec<i64> = match rule.freq.as_str() {
        "YEARLY" => {
            let y = start.0 + step as i32;
            let first = day_number((y, 1, 1));
            let last = first + i64::from(days_in_year(y)) - 1;
            if !rule.by_week_no.is_empty() {
                week_number_days(rule, y, start)
            } else if !rule.by_year_day.is_empty() {
                let mut days: Vec<i64> = rule
                    .by_year_day
                    .iter()
                    .filter_map(|&n| {
                        let k = if n > 0 { n } else { days_in_year(y) + n + 1 };
                        (1..=days_in_year(y)).contains(&k).then(|| first + i64::from(k) - 1)
                    })
                    .collect();
                days.retain(|&n| passes(rule, from_day_number(n), true, true));
                days
            } else if !rule.by_day.is_empty() && rule.by_month.is_empty() && rule.by_month_day.is_empty() {
                // Weekdays of the whole year (numbered within the year).
                rule.by_day.iter().flat_map(|&w| weekdays_in(first, last, w)).collect()
            } else {
                let months = if rule.by_month.is_empty() { vec![start.1] } else { rule.by_month.clone() };
                months.iter().flat_map(|&m| month_dates(rule, y, m, start)).collect()
            }
        }
        "MONTHLY" => {
            let months = i64::from(start.0) * 12 + i64::from(start.1 - 1) + step;
            let (y, m) = (months.div_euclid(12) as i32, months.rem_euclid(12) as i32 + 1);
            if !rule.by_month.is_empty() && !rule.by_month.contains(&m) {
                Vec::new()
            } else {
                month_dates(rule, y, m, start)
            }
        }
        "WEEKLY" => {
            // The week (from WKST) holding the start, `step` weeks on.
            let s = day_number(start);
            let week = s - i64::from((weekday(start) - rule.week_start).rem_euclid(7)) + 7 * step;
            let wanted: Vec<i32> = if rule.by_day.is_empty() { vec![weekday(start)] } else { rule.by_day.iter().map(|w| w.day).collect() };
            (week..week + 7).filter(|&n| wanted.contains(&weekday(from_day_number(n))) && passes(rule, from_day_number(n), false, false)).collect()
        }
        "DAILY" => {
            let n = day_number(start) + step;
            if passes(rule, from_day_number(n), true, true) { vec![n] } else { Vec::new() }
        }
        _ => Vec::new(),
    };
    days.sort_unstable();
    days.dedup();
    days
}

/// A time of day: hour, minute, second.
type Clock = (i32, i32, i32);

/// The instants (day number, time) of period `k`: its dates at the
/// `BYHOUR`, `BYMINUTE` and `BYSECOND` times (else the start's time),
/// then `BYSETPOS`.
fn period_instants(rule: &Rule, start: Date, at: Clock, k: i64) -> Vec<(i64, Clock)> {
    let or = |list: &Vec<i32>, own: i32| if list.is_empty() { vec![own] } else { list.clone() };
    let (hours, minutes, seconds) = (or(&rule.by_hour, at.0), or(&rule.by_minute, at.1), or(&rule.by_second, at.2));
    let mut all: Vec<(i64, Clock)> = Vec::new();
    for day in period_dates(rule, start, k) {
        for &h in &hours {
            for &m in &minutes {
                for &s in &seconds {
                    all.push((day, (h, m, s)));
                }
            }
        }
    }
    all.sort_unstable();
    all.dedup();
    if rule.by_set_pos.is_empty() {
        return all;
    }
    let mut picked: Vec<(i64, Clock)> = rule
        .by_set_pos
        .iter()
        .filter_map(|&p| if p > 0 { all.get(p as usize - 1) } else { all.len().checked_sub(p.unsigned_abs() as usize).and_then(|i| all.get(i)) })
        .copied()
        .collect();
    picked.sort_unstable();
    picked.dedup();
    picked
}

/// Whether an hourly or minutely occurrence's time passes the time parts
/// (which only narrow those rules).
fn time_passes(rule: &Rule, (h, m, s): Clock) -> bool {
    (rule.by_hour.is_empty() || rule.by_hour.contains(&h)) && (rule.by_minute.is_empty() || rule.by_minute.contains(&m)) && (rule.by_second.is_empty() || rule.by_second.contains(&s))
}

/// How long before the range an occurrence may start and still be kept
/// (it may last into the range).
const KEEP_BEFORE: i64 = 366 * 86_400;

/// Start times of a recurring event from `start` that may overlap
/// `[from, to)`. The start is always the first occurrence (and counts for
/// `COUNT`).
fn expand(start: &DateTime, rule: &Rule, from: i64, to: i64) -> Vec<DateTime> {
    const LIMIT: usize = 5000;
    const PERIODS: i64 = 100_000;
    let first = start.to_unix();
    let mut out = vec![start.clone()];
    if rule.count == Some(1) {
        return out;
    }
    let mut produced = 1u32;
    let mut push = |dt: DateTime, out: &mut Vec<DateTime>| -> bool {
        let t = dt.to_unix();
        if rule.until.is_some_and(|u| t > u) || rule.count.is_some_and(|c| produced >= c) || t >= to || out.len() >= LIMIT {
            return false;
        }
        if t > first {
            produced += 1;
            if t >= from - KEEP_BEFORE {
                out.push(dt);
            }
        }
        true
    };
    match rule.freq.as_str() {
        "HOURLY" | "MINUTELY" | "SECONDLY" => {
            let unit = match rule.freq.as_str() {
                "HOURLY" => 3600,
                "MINUTELY" => 60,
                _ => 1,
            };
            for k in 1..PERIODS {
                let t = first + k * i64::from(rule.interval) * unit;
                let Ok(dt) = start.add_seconds((t - first) as f64) else { break };
                let Ok(local) = dt.to_timezone(&start.timezone()) else { break };
                let date = (local.year(), local.month(), local.day_of_month());
                let clock = (local.hour(), local.minute(), local.second());
                if passes(rule, date, true, true) && time_passes(rule, clock) && !push(local, &mut out) {
                    break;
                }
            }
        }
        "YEARLY" | "MONTHLY" | "WEEKLY" | "DAILY" => {
            let tz = start.timezone();
            let date = (start.year(), start.month(), start.day_of_month());
            let (h, mi, sec) = (start.hour(), start.minute(), start.seconds());
            'periods: for k in 0..PERIODS {
                for (n, (hh, mm, ss)) in period_instants(rule, date, (h, mi, sec as i32), k) {
                    let (y, m, d) = from_day_number(n);
                    let Ok(dt) = DateTime::new(&tz, y, m, d, hh, mm, f64::from(ss)) else { continue };
                    if !push(dt, &mut out) {
                        break 'periods;
                    }
                }
                // Stop once a whole period lies after `to`.
                let Ok(probe) = DateTime::new(&tz, date.0, date.1, date.2, h, mi, sec) else { break };
                let ahead = match rule.freq.as_str() {
                    "YEARLY" => probe.add_years(((k + 1) * i64::from(rule.interval) - 1) as i32),
                    "MONTHLY" => probe.add_months(((k + 1) * i64::from(rule.interval) - 1) as i32),
                    "WEEKLY" => probe.add_weeks(((k + 1) * i64::from(rule.interval) - 1) as i32),
                    _ => probe.add_days(((k + 1) * i64::from(rule.interval) - 1) as i32),
                };
                if ahead.map_or(true, |a| a.to_unix() >= to + 400 * 86_400) {
                    break;
                }
            }
        }
        _ => {}
    }
    out
}

/// The occurrences, overlapping `[from, to)`, of the events in `ics`
/// texts (as EDS returns them).
pub fn occurrences(ics: &[String], from: i64, to: i64) -> Vec<Occurrence> {
    let events: Vec<Event> = ics.iter().flat_map(|t| parse_events(t)).collect();
    // Detached instances replace their master's occurrence.
    let detached: HashSet<(String, i64)> = events
        .iter()
        .filter_map(|e| Some((e.uid.clone(), e.recurrence_id.as_ref()?.datetime()?.to_unix())))
        .collect();
    let mut out = Vec::new();
    for e in events.iter().filter(|e| !e.cancelled) {
        let Some(start) = e.start.as_ref() else { continue };
        let Some(begin) = start.datetime() else { continue };
        let all_day = start.time.is_none();
        // Length: in days for all-day events (so DST does not shift them).
        let (days, seconds) = match (&e.end, e.duration) {
            (Some(end), _) => match (all_day, end.datetime()) {
                (true, Some(end)) => (((end.to_unix() - begin.to_unix() + 43_200) / 86_400) as i32, 0),
                (false, Some(end)) => (0, end.to_unix() - begin.to_unix()),
                (_, None) => (i32::from(all_day), 0),
            },
            (None, Some(d)) if all_day => ((d / 86_400) as i32, 0),
            (None, Some(d)) => (0, d),
            (None, None) => (i32::from(all_day), 0),
        };
        let recurring = e.recurrence_id.is_none() && (e.rrule.is_some() || !e.rdates.is_empty());
        let mut starts = match (&e.rrule, &e.recurrence_id) {
            (Some(rule), None) => expand(&begin, &parse_rule(rule), from, to),
            _ => vec![begin.clone()],
        };
        if e.recurrence_id.is_none() {
            // Extra dates (all-day ones at local midnight), not counted by COUNT.
            starts.extend(e.rdates.iter().filter_map(|t| match (all_day, t.time) {
                (false, None) => DateTime::new(&begin.timezone(), t.date.0, t.date.1, t.date.2, begin.hour(), begin.minute(), begin.seconds()).ok(),
                _ => t.datetime(),
            }));
            starts.sort_by_key(|d| d.to_unix());
            starts.dedup_by_key(|d| d.to_unix());
        }
        let mut excluded: HashSet<i64> = e.exdates.iter().filter_map(|t| t.datetime()).map(|d| d.to_unix()).collect();
        for rule in &e.exrules {
            excluded.extend(expand(&begin, &parse_rule(rule), from, to).iter().map(|d| d.to_unix()));
        }
        for s in starts {
            let unix = s.to_unix();
            if recurring && (excluded.contains(&unix) || detached.contains(&(e.uid.clone(), unix))) {
                continue;
            }
            let end = if all_day { s.add_days(days.max(1)).map_or(unix + 86_400, |d| d.to_unix()) } else { unix + seconds.max(0) };
            let o = Occurrence { start: unix, end, all_day, summary: e.summary.clone() };
            if o.overlaps(from, to) {
                out.push(o);
            }
        }
    }
    out.sort_by_key(|a| (!a.all_day, a.start));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(y: i32, m: i32, d: i32, h: i32, mi: i32) -> i64 {
        DateTime::new(&TimeZone::utc(), y, m, d, h, mi, 0.0).unwrap().to_unix()
    }

    fn cal(body: &str) -> Vec<String> {
        vec![format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{body}END:VCALENDAR\r\n")]
    }

    #[test]
    fn single_events() {
        let ics = cal(
            "BEGIN:VEVENT\r\nUID:a\r\nDTSTART:20261112T140000Z\r\nDTEND:20261112T150000Z\r\nSUMMARY:Team\\, weekly\r\n BRAINSTORM\r\nBEGIN:VALARM\r\nSUMMARY:not this\r\nEND:VALARM\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:b\r\nDTSTART;TZID=/freeassociation.sourceforge.net/Europe/Berlin:20261113T090000\r\nDURATION:PT30M\r\nSUMMARY:Call\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:c\r\nDTSTART:20261201T100000Z\r\nSUMMARY:Later\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:d\r\nDTSTART:20261112T100000Z\r\nSTATUS:CANCELLED\r\nSUMMARY:Off\r\nEND:VEVENT\r\n",
        );
        let o = occurrences(&ics, utc(2026, 11, 1, 0, 0), utc(2026, 11, 30, 0, 0));
        assert_eq!(o.len(), 2);
        assert_eq!((o[0].start, o[0].end), (utc(2026, 11, 12, 14, 0), utc(2026, 11, 12, 15, 0)));
        assert_eq!(o[0].summary, "Team, weeklyBRAINSTORM");
        assert_eq!((o[1].start, o[1].end - o[1].start), (utc(2026, 11, 13, 8, 0), 1800), "Berlin is UTC+1 in November");
    }

    #[test]
    fn recurrences() {
        let ics = cal(
            "BEGIN:VEVENT\r\nUID:w\r\nDTSTART:20261102T120000Z\r\nDTEND:20261102T130000Z\r\nRRULE:FREQ=WEEKLY;BYDAY=MO,WE;COUNT=5\r\nEXDATE:20261104T120000Z\r\nSUMMARY:Gym\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:w\r\nRECURRENCE-ID:20261109T120000Z\r\nDTSTART:20261109T170000Z\r\nDTEND:20261109T180000Z\r\nSUMMARY:Gym (moved)\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:m\r\nDTSTART:20260131T080000Z\r\nRRULE:FREQ=MONTHLY;UNTIL=20260601T000000Z\r\nSUMMARY:Rent\r\nEND:VEVENT\r\n",
        );
        let o = occurrences(&ics, utc(2026, 1, 1, 0, 0), utc(2026, 12, 31, 0, 0));
        let gym: Vec<i64> = o.iter().filter(|x| x.summary == "Gym").map(|x| x.start).collect();
        // Mo 2, (We 4 excluded), (Mo 9 moved), We 11, Mo 16: five generated.
        assert_eq!(gym, [utc(2026, 11, 2, 12, 0), utc(2026, 11, 11, 12, 0), utc(2026, 11, 16, 12, 0)]);
        assert!(o.iter().any(|x| x.summary == "Gym (moved)" && x.start == utc(2026, 11, 9, 17, 0)));
        let rent: Vec<i64> = o.iter().filter(|x| x.summary == "Rent").map(|x| x.start).collect();
        assert_eq!(rent, [utc(2026, 1, 31, 8, 0), utc(2026, 3, 31, 8, 0), utc(2026, 5, 31, 8, 0)], "no 31st in Feb or Apr");
    }

    /// Start times (UTC) of `rule` from `dtstart` (UTC) within 1997–2031.
    fn starts(dtstart: &str, rule: &str) -> Vec<i64> {
        let ics = cal(&format!("BEGIN:VEVENT\r\nUID:r\r\nDTSTART:{dtstart}\r\nRRULE:{rule}\r\nSUMMARY:x\r\nEND:VEVENT\r\n"));
        occurrences(&ics, utc(1997, 1, 1, 0, 0), utc(2031, 1, 1, 0, 0)).iter().map(|o| o.start).collect()
    }

    fn days(list: &[(i32, i32, i32)], h: i32) -> Vec<i64> {
        list.iter().map(|&(y, m, d)| utc(y, m, d, h, 0)).collect()
    }

    #[test]
    fn rule_parts() {
        // Numbered weekdays in a month.
        assert_eq!(starts("20260113T100000Z", "FREQ=MONTHLY;BYDAY=2TU;COUNT=4"), days(&[(2026, 1, 13), (2026, 2, 10), (2026, 3, 10), (2026, 4, 14)], 10));
        assert_eq!(starts("20260130T100000Z", "FREQ=MONTHLY;BYDAY=-1FR;COUNT=3"), days(&[(2026, 1, 30), (2026, 2, 27), (2026, 3, 27)], 10));
        // Days from the end of the month.
        assert_eq!(starts("20260131T090000Z", "FREQ=MONTHLY;BYMONTHDAY=-1;COUNT=3"), days(&[(2026, 1, 31), (2026, 2, 28), (2026, 3, 31)], 9));
        // The last weekday of the month.
        assert_eq!(
            starts("20260130T090000Z", "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1;COUNT=3"),
            days(&[(2026, 1, 30), (2026, 2, 27), (2026, 3, 31)], 9)
        );
        // The fourth Thursday of November.
        assert_eq!(starts("20261126T170000Z", "FREQ=YEARLY;BYMONTH=11;BYDAY=4TH;COUNT=2"), days(&[(2026, 11, 26), (2027, 11, 25)], 17));
        // RFC 5545 examples: every 20th Monday of the year; day 100.
        assert_eq!(starts("19970519T090000Z", "FREQ=YEARLY;BYDAY=20MO;COUNT=3"), days(&[(1997, 5, 19), (1998, 5, 18), (1999, 5, 17)], 9));
        assert_eq!(starts("19970410T090000Z", "FREQ=YEARLY;BYYEARDAY=100;COUNT=2"), days(&[(1997, 4, 10), (1998, 4, 10)], 9));
        // The week start changes which weeks INTERVAL=2 picks.
        assert_eq!(starts("19970805T090000Z", "FREQ=WEEKLY;INTERVAL=2;COUNT=4;BYDAY=TU,SU;WKST=MO"), days(&[(1997, 8, 5), (1997, 8, 10), (1997, 8, 19), (1997, 8, 24)], 9));
        assert_eq!(starts("19970805T090000Z", "FREQ=WEEKLY;INTERVAL=2;COUNT=4;BYDAY=TU,SU;WKST=SU"), days(&[(1997, 8, 5), (1997, 8, 17), (1997, 8, 19), (1997, 8, 31)], 9));
        // Every day in January, for two years.
        let jan = starts("19980101T090000Z", "FREQ=DAILY;UNTIL=20000131T140000Z;BYMONTH=1");
        assert_eq!((jan.len(), jan[31]), (93, utc(1999, 1, 1, 9, 0)));
        // The 13th when it is a Friday (DTSTART itself counts first).
        assert_eq!(starts("19970902T090000Z", "FREQ=MONTHLY;BYDAY=FR;BYMONTHDAY=13;COUNT=3"), days(&[(1997, 9, 2), (1998, 2, 13), (1998, 3, 13)], 9));
        // Every 3 hours, then every 90 minutes.
        assert_eq!(starts("19970902T090000Z", "FREQ=HOURLY;INTERVAL=3;COUNT=3"), [utc(1997, 9, 2, 9, 0), utc(1997, 9, 2, 12, 0), utc(1997, 9, 2, 15, 0)]);
        assert_eq!(starts("19970902T090000Z", "FREQ=MINUTELY;INTERVAL=90;COUNT=2"), [utc(1997, 9, 2, 9, 0), utc(1997, 9, 2, 10, 30)]);
        // Times of day: RFC 5545's "every 20 minutes from 9:00 to 9:40, today and tomorrow".
        assert_eq!(
            starts("19970902T090000Z", "FREQ=DAILY;BYHOUR=9;BYMINUTE=0,20,40;COUNT=6"),
            [utc(1997, 9, 2, 9, 0), utc(1997, 9, 2, 9, 20), utc(1997, 9, 2, 9, 40), utc(1997, 9, 3, 9, 0), utc(1997, 9, 3, 9, 20), utc(1997, 9, 3, 9, 40)]
        );
        // Hourly, only at 9 and 10.
        assert_eq!(starts("19970902T090000Z", "FREQ=HOURLY;BYHOUR=9,10;COUNT=4"), [utc(1997, 9, 2, 9, 0), utc(1997, 9, 2, 10, 0), utc(1997, 9, 3, 9, 0), utc(1997, 9, 3, 10, 0)]);
        // Week numbers (RFC 5545): Monday of week 20; Monday of week 1 in 1998 is in 1997.
        assert_eq!(starts("19970512T090000Z", "FREQ=YEARLY;BYWEEKNO=20;BYDAY=MO;COUNT=3"), days(&[(1997, 5, 12), (1998, 5, 11), (1999, 5, 17)], 9));
        assert_eq!(starts("19971229T090000Z", "FREQ=YEARLY;BYWEEKNO=1;BYDAY=MO;COUNT=2"), days(&[(1997, 12, 29), (1999, 1, 4)], 9));
        // The last week of the year.
        assert_eq!(starts("19971222T090000Z", "FREQ=YEARLY;BYWEEKNO=-1;BYDAY=MO;COUNT=2"), days(&[(1997, 12, 22), (1998, 12, 28)], 9));
        // Every 20 seconds, four times.
        let at = utc(1997, 9, 2, 9, 0);
        assert_eq!(starts("19970902T090000Z", "FREQ=SECONDLY;INTERVAL=20;COUNT=4"), [at, at + 20, at + 40, at + 60]);
        // EXRULE takes out what it matches (here every other day).
        let ics = cal("BEGIN:VEVENT\r\nUID:x\r\nDTSTART:20261102T080000Z\r\nRRULE:FREQ=DAILY;COUNT=6\r\nEXRULE:FREQ=DAILY;INTERVAL=2;COUNT=3\r\nSUMMARY:x\r\nEND:VEVENT\r\n");
        let o = occurrences(&ics, utc(2026, 11, 1, 0, 0), utc(2026, 11, 30, 0, 0));
        assert_eq!(o.iter().map(|o| o.start).collect::<Vec<_>>(), [utc(2026, 11, 3, 8, 0), utc(2026, 11, 5, 8, 0), utc(2026, 11, 7, 8, 0)]);
        // A daily event from long ago still reaches the range.
        let ics = cal("BEGIN:VEVENT\r\nUID:old\r\nDTSTART:20000101T070000Z\r\nRRULE:FREQ=DAILY\r\nSUMMARY:x\r\nEND:VEVENT\r\n");
        let o = occurrences(&ics, utc(2026, 11, 12, 0, 0), utc(2026, 11, 13, 0, 0));
        assert_eq!(o.iter().map(|o| o.start).collect::<Vec<_>>(), [utc(2026, 11, 12, 7, 0)]);
    }

    #[test]
    fn extra_dates() {
        let ics = cal(
            "BEGIN:VEVENT\r\nUID:x\r\nDTSTART:20261102T120000Z\r\nDURATION:PT1H\r\nRRULE:FREQ=WEEKLY;COUNT=2\r\n\
             RDATE:20261104T150000Z,20261120T150000Z/PT1H\r\nEXDATE:20261120T150000Z\r\nSUMMARY:Class\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:y\r\nDTSTART;VALUE=DATE:20261105\r\nRDATE;VALUE=DATE:20261112\r\nSUMMARY:Bins\r\nEND:VEVENT\r\n",
        );
        let o = occurrences(&ics, utc(2026, 11, 1, 0, 0), utc(2026, 12, 1, 0, 0));
        let class: Vec<i64> = o.iter().filter(|x| x.summary == "Class").map(|x| x.start).collect();
        assert_eq!(class, [utc(2026, 11, 2, 12, 0), utc(2026, 11, 4, 15, 0), utc(2026, 11, 9, 12, 0)]);
        assert_eq!(o.iter().filter(|x| x.summary == "Bins" && x.all_day).count(), 2);
    }

    #[test]
    fn dates() {
        for n in [-800_000, -1, 0, 59, 10_957, 20_404, 400_000] {
            assert_eq!(day_number(from_day_number(n)), n);
        }
        assert_eq!(weekday((2026, 11, 12)), 4, "a Thursday");
        assert_eq!(year_day((2024, 12, 31)), 366);
        assert_eq!(weekdays_in(day_number((2026, 11, 1)), day_number((2026, 11, 30)), WeekdayNum { nth: -1, day: 1 }), [day_number((2026, 11, 30))]);
        let r = parse_rule("FREQ=MONTHLY;BYDAY=+2TU,-1fr,SU,9XX;BYMONTHDAY=-1,32;WKST=SU");
        assert_eq!(r.by_day, [WeekdayNum { nth: 2, day: 2 }, WeekdayNum { nth: -1, day: 5 }, WeekdayNum { nth: 0, day: 7 }]);
        assert_eq!((r.by_month_day, r.week_start), (vec![-1], 7));
    }

    #[test]
    fn all_day() {
        let ics = cal("BEGIN:VEVENT\r\nUID:h\r\nDTSTART;VALUE=DATE:20261113\r\nDTEND;VALUE=DATE:20261115\r\nSUMMARY:Trip\r\nEND:VEVENT\r\n");
        let o = occurrences(&ics, 0, i64::MAX / 2);
        let local = |d| DateTime::new(&TimeZone::local(), 2026, 11, d, 0, 0, 0.0).unwrap().to_unix();
        assert_eq!((o[0].start, o[0].end, o[0].all_day), (local(13), local(15), true));
        assert!(o[0].overlaps(local(14), local(15)));
        assert!(!o[0].overlaps(local(15), local(16)));
        assert_eq!(parse_duration("-P1DT2H"), Some(-93_600));
        assert_eq!(parse_duration("P2W"), Some(1_209_600));
    }
}
