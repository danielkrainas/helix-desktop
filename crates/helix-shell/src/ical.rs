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
    exdates: Vec<Time>,
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
                    "EXDATE" => e.exdates.extend(value.split(',').filter_map(|v| parse_time(&params, v))),
                    "RECURRENCE-ID" => e.recurrence_id = parse_time(&params, &value),
                    "STATUS" => e.cancelled = value.eq_ignore_ascii_case("CANCELLED"),
                    _ => {}
                }
            }
        }
    }
    out
}

#[derive(Debug, Default)]
struct Rule {
    freq: String,
    interval: i32,
    count: Option<u32>,
    until: Option<i64>,
    /// Weekdays (1 = Monday … 7 = Sunday) for weekly rules.
    by_day: Vec<i32>,
}

fn parse_rule(s: &str) -> Rule {
    let mut rule = Rule { interval: 1, ..Rule::default() };
    for part in s.split(';') {
        let Some((k, v)) = part.split_once('=') else { continue };
        match k.to_ascii_uppercase().as_str() {
            "FREQ" => rule.freq = v.to_ascii_uppercase(),
            "INTERVAL" => rule.interval = v.parse().unwrap_or(1).max(1),
            "COUNT" => rule.count = v.parse().ok(),
            "UNTIL" => rule.until = parse_time(&[], v).and_then(|t| t.datetime()).map(|d| d.to_unix()),
            "BYDAY" => {
                const DAYS: [&str; 7] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];
                // Plain weekdays only (no "1MO").
                rule.by_day = v.split(',').filter_map(|d| DAYS.iter().position(|x| *x == d).map(|i| i as i32 + 1)).collect();
            }
            _ => {}
        }
    }
    rule
}

/// Start times of a recurring event from `start`, up to `to`.
fn expand(start: &DateTime, rule: &Rule, to: i64) -> Vec<DateTime> {
    const LIMIT: usize = 5000;
    let mut out = Vec::new();
    let mut produced = 0u32;
    let day = start.day_of_month();
    let mut push = |dt: DateTime, out: &mut Vec<DateTime>| -> bool {
        if rule.until.is_some_and(|u| dt.to_unix() > u) || rule.count.is_some_and(|c| produced >= c) || dt.to_unix() >= to {
            return false;
        }
        produced += 1;
        out.push(dt);
        true
    };
    for k in 0..LIMIT as i32 {
        let step = k * rule.interval;
        let next = match rule.freq.as_str() {
            "DAILY" => start.add_days(step).ok(),
            "WEEKLY" if !rule.by_day.is_empty() => {
                // The week of `start` (from Monday), then each listed day.
                let Ok(week) = start.add_days(1 - start.day_of_week()).and_then(|m| m.add_weeks(step)) else { break };
                let mut days: Vec<i32> = rule.by_day.clone();
                days.sort_unstable();
                for d in days {
                    let Ok(dt) = week.add_days(d - 1) else { continue };
                    if dt.to_unix() < start.to_unix() {
                        continue;
                    }
                    if !push(dt, &mut out) {
                        return out;
                    }
                }
                if out.len() >= LIMIT {
                    break;
                }
                continue;
            }
            "WEEKLY" => start.add_weeks(step).ok(),
            // Months or years without that day are skipped.
            "MONTHLY" => start.add_months(step).ok().filter(|d| d.day_of_month() == day),
            "YEARLY" => start.add_years(step).ok().filter(|d| d.day_of_month() == day),
            _ => {
                push(start.clone(), &mut out);
                return out;
            }
        };
        let Some(next) = next else { continue };
        if !push(next, &mut out) || out.len() >= LIMIT {
            break;
        }
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
        let starts = match (&e.rrule, &e.recurrence_id) {
            (Some(rule), None) => expand(&begin, &parse_rule(rule), to),
            _ => vec![begin.clone()],
        };
        let excluded: HashSet<i64> = e.exdates.iter().filter_map(|t| t.datetime()).map(|d| d.to_unix()).collect();
        for s in starts {
            let unix = s.to_unix();
            if e.rrule.is_some() && (excluded.contains(&unix) || detached.contains(&(e.uid.clone(), unix))) {
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
