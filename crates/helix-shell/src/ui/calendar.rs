//! The message tray's calendar column (helixdesktop/spec
//! `contracts/gsettings/calendar.md`): date, month grid, the chosen day's
//! events, world clocks and the media player.

use gio::glib::{DateTime, TimeZone};
use helix_draw::Image;

use super::notify::ListHit;
use super::*;
use crate::calendar::CalendarData;
use crate::shell_keys::Playback;
use helix_i18n::{tr, trc};
use helix_i18n::trf;

/// The column's width: it grows with the text scale.
pub(super) fn cal_w() -> i32 {
    helix_draw::theme::grow(320)
}
const PAD: i32 = 14;
const HEADER_H: i32 = 56;
const MONTH_H: i32 = 32;
const WEEKDAYS_H: i32 = 22;
const CELL_H: i32 = 32;
const LINE_H: i32 = 22;
const SECTION_H: i32 = 26;
const GAP: i32 = 10;
const MAX_EVENTS: usize = 6;
const MEDIA_H: i32 = 64 + 40;

/// The first day of the week in the locale (1 = Monday … 7 = Sunday),
/// read from the C library's locale data (`LC_TIME`).
pub(super) fn locale_week_start() -> i32 {
    helix_i18n::first_weekday() as i32 + 1
}

fn show_week_numbers() -> bool {
    const SCHEMA: &str = "org.gnome.desktop.calendar";
    helix_settings::has_schema(SCHEMA) && helix_settings::Settings::new(SCHEMA).boolean("show-weekdate")
}

fn today() -> (i32, i32, i32) {
    DateTime::now_local().map(|d| d.ymd()).unwrap_or((1970, 1, 1))
}

fn local_date(y: i32, m: i32, d: i32) -> Option<DateTime> {
    DateTime::new(&TimeZone::local(), y, m, d, 0, 0, 0.0).ok()
}

fn fmt(d: &DateTime, f: &str) -> String {
    d.format(f).map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default()
}

pub(super) struct CalendarState {
    /// The month shown: (year, month).
    pub month: (i32, i32),
    pub selected: (i32, i32, i32),
    pub data: Option<CalendarData>,
    /// The range last asked for.
    pub asked: (i64, i64),
    week_start: Option<i32>,
    show_weeks: bool,
}

impl Default for CalendarState {
    fn default() -> Self {
        let (y, m, d) = today();
        Self { month: (y, m), selected: (y, m, d), data: None, asked: (0, 0), week_start: None, show_weeks: false }
    }
}

impl CalendarState {
    /// This state with the locale's first weekday, as the shell sets it
    /// when the list opens (for render galleries).
    #[cfg(test)]
    pub(in crate::ui) fn with_locale_week_start(mut self) -> Self {
        self.week_start = Some(locale_week_start());
        self
    }

    /// The first day the grid shows (local midnight).
    fn first_shown(&self) -> Option<DateTime> {
        let first = local_date(self.month.0, self.month.1, 1)?;
        let start = self.week_start.unwrap_or(7);
        first.add_days(-(first.day_of_week() - start).rem_euclid(7)).ok()
    }

    /// The grid's range in Unix seconds.
    fn range(&self) -> Option<(i64, i64)> {
        let first = self.first_shown()?;
        Some((first.to_unix(), first.add_days(42).ok()?.to_unix()))
    }

    /// The month shown in words ("November 2026").
    pub(super) fn month_title(&self) -> String {
        // Translators: a month and year heading (GLib DateTime format).
        local_date(self.month.0, self.month.1, 1).map(|d| fmt(&d, &trc("month and year", "%OB %Y"))).unwrap_or_default()
    }

    /// The selected day's events as read out: each one's summary, and
    /// when it is and how long it lasts ("10:00 to 11:30, 1 hour 30
    /// minutes"; "All day"; "All day, 3 days").
    pub(super) fn selected_events(&self) -> Vec<(String, String)> {
        let format = crate::clock::current();
        let time = |t: i64| DateTime::from_unix_local(t).map(|d| crate::clock::time_of_day(format, &d)).unwrap_or_default();
        self.day_events(self.selected).iter().map(|e| (e.summary.clone(), event_when(e, &time))).collect()
    }

    fn day_events(&self, (y, m, d): (i32, i32, i32)) -> Vec<&crate::ical::Occurrence> {
        let (Some(start), Some(data)) = (local_date(y, m, d), self.data.as_ref()) else { return Vec::new() };
        let Ok(end) = start.add_days(1) else { return Vec::new() };
        data.events.iter().filter(|e| e.overlaps(start.to_unix(), end.to_unix())).collect()
    }
}

/// The media buttons' icons (previous, play or pause, next), then the
/// weather's.
pub(super) type MediaIcons = [Option<Image>; 4];

impl Ui {
    /// The tray opened: today, this month, fresh data.
    pub(super) fn calendar_open(&mut self) {
        let week_start = self.calendar.week_start.unwrap_or_else(locale_week_start);
        let (y, m, d) = today();
        self.calendar = CalendarState { month: (y, m), selected: (y, m, d), week_start: Some(week_start), show_weeks: show_week_numbers(), ..CalendarState::default() };
        self.calendar_request();
    }

    fn calendar_request(&mut self) {
        let Some((from, to)) = self.calendar.range() else { return };
        self.calendar.asked = (from, to);
        let _ = self.requests.send(UiRequest::Calendar { from, to });
    }

    /// New data from the D-Bus side.
    pub(super) fn calendar_data(&mut self, data: CalendarData) {
        if (data.from, data.to) != self.calendar.asked {
            return;
        }
        self.calendar.data = Some(data);
        self.draw_notification_list();
    }

    pub(super) fn calendar_click(&mut self, hit: &ListHit) {
        match *hit {
            ListHit::Month(delta) => {
                let (y, m) = self.calendar.month;
                let index = y * 12 + (m - 1) + delta;
                self.calendar.month = (index.div_euclid(12), index.rem_euclid(12) + 1);
                self.calendar_request();
            }
            ListHit::Day(y, m, d) => {
                self.calendar.selected = (y, m, d);
                if (y, m) != self.calendar.month {
                    self.calendar.month = (y, m);
                    self.calendar_request();
                }
            }
            ListHit::Media(command) => {
                let (from, to) = self.calendar.asked;
                let _ = self.requests.send(UiRequest::Media { command, from, to });
            }
            _ => return,
        }
        self.draw_notification_list();
    }

    /// The keyboard moved the selected day (the month follows it).
    pub(super) fn calendar_move(&mut self, by: CalendarMove) {
        let Some((y, m, d)) = moved(self.calendar.selected, by) else { return };
        self.calendar.selected = (y, m, d);
        if (y, m) != self.calendar.month {
            self.calendar.month = (y, m);
            self.calendar_request();
        }
        if let Some(view) = self.notify.view.as_mut() {
            super::notify::view_focus_day(view, (y, m, d));
        }
        self.draw_notification_list();
    }

    pub(super) fn calendar_media_icons(&mut self, scale: f64) -> MediaIcons {
        let playing = self.calendar.data.as_ref().and_then(|d| d.player.as_ref()).is_some_and(|p| p.playing);
        let play = if playing { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" };
        let weather = self.calendar.data.as_ref().and_then(|d| d.weather.clone().flatten()).map(|f| crate::weather::condition(&f.symbol).1);
        [
            self.symbolic_icon("media-skip-backward-symbolic", scale),
            self.symbolic_icon(play, scale),
            self.symbolic_icon("media-skip-forward-symbolic", scale),
            weather.and_then(|icon| self.symbolic_icon(icon, scale)),
        ]
    }

    pub(super) fn calendar_height(&self) -> i32 {
        height(&self.calendar)
    }
}

/// The column's height.
pub(super) fn height(cal: &CalendarState) -> i32 {
    let grow = helix_draw::theme::grow;
    let events = cal.day_events(cal.selected).len();
    let event_lines = if events == 0 { 1 } else { events.min(MAX_EVENTS) as i32 + i32::from(events > MAX_EVENTS) };
    let mut h = grow(HEADER_H) + grow(MONTH_H) + grow(WEEKDAYS_H) + 6 * grow(CELL_H) + GAP + grow(SECTION_H) + event_lines * grow(LINE_H);
    if let Some(data) = &cal.data {
        if !data.clocks.is_empty() {
            h += GAP + grow(SECTION_H) + data.clocks.len() as i32 * grow(LINE_H);
        }
        if data.weather.is_some() {
            h += GAP + grow(SECTION_H) + 2 * grow(LINE_H) + 6;
        }
        if data.player.is_some() {
            h += GAP + grow(MEDIA_H);
        }
    }
    h + GAP
}

/// How the keyboard moves the selected day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CalendarMove {
    Days(i32),
    /// To the same day of another month (or its last day).
    Months(i32),
    Today,
}

/// The day `by` from `selected`.
pub(super) fn moved(selected: (i32, i32, i32), by: CalendarMove) -> Option<(i32, i32, i32)> {
    match by {
        CalendarMove::Today => Some(today()),
        CalendarMove::Days(n) => Some(local_date(selected.0, selected.1, selected.2)?.add_days(n).ok()?.ymd()),
        CalendarMove::Months(n) => {
            let index = selected.0 * 12 + (selected.1 - 1) + n;
            let (y, m) = (index.div_euclid(12), index.rem_euclid(12) + 1);
            let last = local_date(y, m, 1)?.add_months(1).ok()?.add_days(-1).ok()?.day_of_month();
            Some((y, m, selected.2.min(last)))
        }
    }
}

/// Draw the column at (x, y); clickable parts go to `hits`.
pub(super) fn paint_calendar(
    cal: &CalendarState,
    (text, bold): (Option<&Text>, Option<&Text>),
    canvas: &mut Canvas,
    (x, y): (i32, i32),
    icons: &MediaIcons,
    hits: &mut Vec<(Rect, ListHit)>,
    look: &dyn Fn(&ListHit) -> theme::Look,
) {
    let grow = helix_draw::theme::grow;
    let Ok(now) = DateTime::now_local() else { return };
    let today = now.ymd();
    let inner = cal_w() - 2 * PAD;
    let mut y = y;
    // Today.
    label(text, canvas, &fmt(&now, &trc("calendar today weekday", "%A")), (x + PAD) as f32, (y + grow(20)) as f32, size::CAPTION, DIM_FG, inner as f32);
    // Translators: today's date over the calendar (GLib DateTime format).
    label(bold.or(text), canvas, &fmt(&now, &trc("calendar today", "%B %e %Y")), (x + PAD) as f32, (y + grow(44)) as f32, size::SUBTITLE, FG, inner as f32);
    y += grow(HEADER_H);
    // The month and its buttons.
    if let Some(first) = local_date(cal.month.0, cal.month.1, 1) {
        label(bold.or(text), canvas, &fmt(&first, &trc("month and year", "%OB %Y")), (x + PAD) as f32, baseline(text, y, grow(MONTH_H), size::LABEL), size::LABEL, FG, (inner - 70) as f32);
    }
    // Previous and next month: round flat buttons with chevrons.
    for (i, (towards, delta)) in [(theme::Towards::Left, -1), (theme::Towards::Right, 1)].into_iter().enumerate() {
        let d = 30;
        let r = Rect { x: x + cal_w() - PAD - 64 + i as i32 * 34, y: y + (grow(MONTH_H) - d) / 2, w: d, h: d };
        let (cx, cy) = (f64::from(r.x) + 15.0, f64::from(r.y) + 15.0);
        let hit = ListHit::Month(delta);
        theme::round_flat(canvas, cx, cy, f64::from(d), look(&hit), false);
        if helix_draw::high_contrast() && !(look(&hit).hovered || look(&hit).pressed) {
            canvas.ring(cx, cy, 14.5, 1.0, 0xFFFF_FFFF);
        }
        theme::chevron(canvas, cx, cy, 10.0, towards, FG);
        hits.push((r, hit));
    }
    y += grow(MONTH_H);
    // Weekday initials, optional week numbers, six weeks of days.
    let weeks_w = if cal.show_weeks { 28 } else { 0 };
    let cw = (inner - weeks_w) / 7;
    let gx = x + PAD + weeks_w;
    let Some(first) = cal.first_shown() else { return };
    for col in 0..7 {
        let Ok(day) = first.add_days(col) else { continue };
        let initial: String = fmt(&day, "%a").chars().take(1).collect();
        let tw = text.map_or(0.0, |t| t.width(&initial, size::CAPTION));
        label(text, canvas, &initial, gx as f32 + (col * cw) as f32 + (cw as f32 - tw) / 2.0, (y + grow(15)) as f32, size::CAPTION, DIM_FG, cw as f32);
    }
    y += grow(WEEKDAYS_H);
    for row in 0..6 {
        let ry = y + row * grow(CELL_H);
        if cal.show_weeks {
            if let Ok(day) = first.add_days(row * 7 + 3) {
                let wk = day.week_of_year().to_string();
                label(text, canvas, &wk, (x + PAD) as f32, baseline(text, ry, grow(CELL_H), size::TINY), size::TINY, DIM_FG, 24.0);
            }
        }
        for col in 0..7 {
            let Ok(day) = first.add_days(row * 7 + col) else { continue };
            let ymd = day.ymd();
            let (cx, cy) = (gx + col * cw + cw / 2, ry + grow(CELL_H) / 2);
            let is_today = ymd == today;
            if is_today {
                canvas.circle(f64::from(cx), f64::from(cy), 13.0, accent_fill());
            } else if ymd == cal.selected {
                if helix_draw::high_contrast() {
                    canvas.ring(f64::from(cx), f64::from(cy), 12.5, 1.5, 0xFFFFFFFF);
                } else {
                    canvas.circle(f64::from(cx), f64::from(cy), 13.0, theme::CONTROL_HOVER);
                }
            } else {
                // Under the pointer: a light circle.
                let l = look(&ListHit::Day(ymd.0, ymd.1, ymd.2));
                theme::flat(canvas, (f64::from(cx) - 13.0, f64::from(cy) - 13.0, 26.0, 26.0), 13.0, l);
            }
            let n = ymd.2.to_string();
            let color = if is_today { on_accent() } else if (ymd.0, ymd.1) == cal.month { FG } else { DIM_FG };
            let tw = text.map_or(0.0, |t| t.width(&n, size::CAPTION));
            label(text, canvas, &n, cx as f32 - tw / 2.0, baseline(text, ry, grow(CELL_H), size::CAPTION) - 2.0, size::CAPTION, color, cw as f32);
            if !cal.day_events(ymd).is_empty() {
                canvas.circle(f64::from(cx), f64::from(cy + 10), 1.8, if is_today { on_accent() } else { DIM_FG });
            }
            hits.push((Rect { x: cx - cw / 2, y: ry, w: cw, h: grow(CELL_H) }, ListHit::Day(ymd.0, ymd.1, ymd.2)));
        }
    }
    y += 6 * grow(CELL_H) + GAP;
    // The chosen day's events.
    let (sy, sm, sd) = cal.selected;
    let heading = match local_date(sy, sm, sd) {
        Some(day) if day.ymd() == today => tr("Today").to_owned(),
        Some(day) if now.add_days(1).is_ok_and(|t| t.ymd() == day.ymd()) => tr("Tomorrow").to_owned(),
        // Translators: the day whose events are listed (GLib DateTime format).
        Some(day) => fmt(&day, &trc("calendar day", "%A, %B %e")),
        None => String::new(),
    };
    theme::separator(canvas, x + PAD, y, inner);
    label(bold.or(text), canvas, &heading, (x + PAD) as f32, (y + grow(19)) as f32, size::CAPTION, FG, inner as f32);
    y += grow(SECTION_H);
    let events = cal.day_events(cal.selected);
    if events.is_empty() {
        label(text, canvas, tr("No Events"), (x + PAD) as f32, (y + grow(16)) as f32, size::CAPTION, DIM_FG, inner as f32);
        y += grow(LINE_H);
    }
    for e in events.iter().take(MAX_EVENTS) {
        let when = if e.all_day {
            tr("All Day").to_owned()
        } else {
            DateTime::from_unix_local(e.start).map(|d| crate::clock::time_of_day(crate::clock::current(), &d)).unwrap_or_default()
        };
        label(text, canvas, &when, (x + PAD) as f32, (y + grow(16)) as f32, size::CAPTION, DIM_FG, (grow(62) - 6) as f32);
        label(text, canvas, &e.summary, (x + PAD + grow(62)) as f32, (y + grow(16)) as f32, size::CAPTION, FG, (inner - grow(62)) as f32);
        y += grow(LINE_H);
    }
    if events.len() > MAX_EVENTS {
        label(text, canvas, &helix_i18n::trnf!("{n} more", "{n} more", events.len() - MAX_EVENTS), (x + PAD + grow(62)) as f32, (y + grow(16)) as f32, size::CAPTION, DIM_FG, inner as f32);
        y += grow(LINE_H);
    }
    let Some(data) = &cal.data else { return };
    // World clocks.
    if !data.clocks.is_empty() {
        y += GAP;
        theme::separator(canvas, x + PAD, y, inner);
        label(bold.or(text), canvas, tr("World Clocks"), (x + PAD) as f32, (y + grow(19)) as f32, size::CAPTION, FG, inner as f32);
        y += grow(SECTION_H);
        for (name, zone) in &data.clocks {
            let there = crate::ical::zone(zone).and_then(|tz| DateTime::now(&tz).ok());
            let time = match &there {
                // Translators: a forecast hour on another day: weekday and time.
                Some(t) if t.ymd() != today => trf!("{weekday} {time}", weekday = fmt(t, "%a"), time = crate::clock::time_of_day(crate::clock::current(), t)),
                Some(t) => crate::clock::time_of_day(crate::clock::current(), t),
                None => String::new(),
            };
            let tw = text.map_or(0.0, |t| t.width(&time, size::CAPTION)) as i32;
            label(text, canvas, name, (x + PAD) as f32, (y + grow(16)) as f32, size::CAPTION, FG, (inner - tw - 8) as f32);
            label(text, canvas, &time, (x + cal_w() - PAD - tw) as f32, (y + grow(16)) as f32, size::CAPTION, DIM_FG, tw as f32 + 1.0);
            y += grow(LINE_H);
        }
    }
    // The weather.
    if let Some(weather) = &data.weather {
        y += GAP;
        theme::separator(canvas, x + PAD, y, inner);
        label(bold.or(text), canvas, tr("Weather"), (x + PAD) as f32, (y + grow(19)) as f32, size::CAPTION, FG, inner as f32);
        y += grow(SECTION_H);
        let unit = data.unit.unwrap_or(crate::weather::Unit::Celsius);
        match weather {
            Some(f) => {
                let (words, _) = crate::weather::condition(&f.symbol);
                if let Some(icon) = &icons[3] {
                    canvas.image(f64::from(x + PAD), f64::from(y + 3), 16.0, 16.0, icon);
                }
                let now = format!("{}  {}, {words}", f.location, crate::weather::temperature_text(f.temperature, unit));
                label(text, canvas, &now, (x + PAD + 24) as f32, (y + grow(16)) as f32, size::CAPTION, FG, (inner - 24) as f32);
                y += grow(LINE_H) + 6;
                let cw = inner / crate::weather::HOURS as i32;
                for (i, (time, temp, _)) in f.hours.iter().enumerate() {
                    let hour = DateTime::from_unix_local(*time).map(|d| crate::clock::time_of_day(crate::clock::current(), &d)).unwrap_or_default();
                    let cell = format!("{hour} {}", crate::weather::temperature_text(*temp, unit));
                    label(text, canvas, &cell, (x + PAD + i as i32 * cw) as f32, (y + grow(16)) as f32, size::CAPTION, DIM_FG, cw as f32);
                }
                y += grow(LINE_H);
            }
            None => {
                label(text, canvas, tr("Weather unavailable"), (x + PAD) as f32, (y + grow(16)) as f32, size::CAPTION, DIM_FG, inner as f32);
                y += 2 * grow(LINE_H) + 6;
            }
        }
    }
    // The media player.
    if let Some(player) = &data.player {
        y += GAP;
        theme::separator(canvas, x + PAD, y, inner);
        label(text, canvas, &player.identity, (x + PAD) as f32, (y + grow(18)) as f32, size::CAPTION, DIM_FG, inner as f32);
        let title = if player.title.is_empty() { tr("Unknown Title") } else { player.title.as_str() };
        label(bold.or(text), canvas, title, (x + PAD) as f32, (y + grow(38)) as f32, size::LABEL, FG, inner as f32);
        label(text, canvas, &player.artist, (x + PAD) as f32, (y + grow(57)) as f32, size::CAPTION, DIM_FG, inner as f32);
        let by = y + 64;
        let commands = [Playback::Previous, Playback::PlayPause, Playback::Next];
        let bw = 44;
        let bx = x + (cal_w() - 3 * bw - 2 * 12) / 2;
        for (i, command) in commands.into_iter().enumerate() {
            let r = Rect { x: bx + i as i32 * (bw + 12), y: by, w: bw, h: 32 };
            // Round buttons; play/pause, the main one, filled.
            let l = theme::Look { on: false, ..look(&ListHit::Media(command)) };
            theme::control_round(canvas, (f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h)), theme::pill(f64::from(r.h)), l);
            if let Some(icon) = &icons[i] {
                let s = 16.0;
                canvas.image(f64::from(r.x) + (f64::from(r.w) - s) / 2.0, f64::from(r.y) + (f64::from(r.h) - s) / 2.0, s, s, icon);
            }
            hits.push((r, ListHit::Media(command)));
        }
    }
}

/// A length of time in words: "1 hour 30 minutes", "45 minutes", "2 hours".
pub(super) fn duration_words(seconds: i64) -> String {
    let minutes = (seconds.max(0) + 30) / 60;
    let (h, m) = (minutes / 60, minutes % 60);
    let hours = helix_i18n::trn("{n} hour", "{n} hours", h as u64);
    let mins = helix_i18n::trn("{n} minute", "{n} minutes", m as u64);
    match (h, m) {
        (0, _) => mins,
        (_, 0) => hours,
        // Translators: a duration, hours then minutes ("1 hour 30 minutes").
        _ => trf!("{hours} {minutes}", hours = hours, minutes = mins),
    }
}

/// When event `e` is, in words, with its times written by `time`: "10:00
/// to 11:30, 1 hour 30 minutes", "10:00" (no end), "All day", "All day, 3
/// days".
pub(super) fn event_when(e: &crate::ical::Occurrence, time: &dyn Fn(i64) -> String) -> String {
    if e.all_day {
        let days = ((e.end - e.start).max(0) + 43_200) / 86_400;
        return if days > 1 { helix_i18n::trnf!("All day, {n} day", "All day, {n} days", days) } else { tr("All Day").to_owned() };
    }
    if e.end <= e.start {
        return time(e.start);
    }
    trf!("{start} to {end}, {duration}", start = time(e.start), end = time(e.end), duration = duration_words(e.end - e.start))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_in_words() {
        let time = |t: i64| format!("{:02}:{:02}", t / 3600 % 24, t / 60 % 60);
        let e = |start, end, all_day| crate::ical::Occurrence { start, end, all_day, summary: "Standup".into() };
        assert_eq!(event_when(&e(36_000, 41_400, false), &time), "10:00 to 11:30, 1 hour 30 minutes");
        assert_eq!(event_when(&e(36_000, 38_700, false), &time), "10:00 to 10:45, 45 minutes");
        assert_eq!(event_when(&e(36_000, 43_200, false), &time), "10:00 to 12:00, 2 hours");
        assert_eq!(event_when(&e(36_000, 36_000, false), &time), "10:00", "no end");
        assert_eq!(event_when(&e(0, 86_400, true), &time), "All Day");
        assert_eq!(event_when(&e(0, 3 * 86_400, true), &time), "All day, 3 days");
    }

    #[test]
    fn keyboard_days() {
        assert_eq!(moved((2026, 11, 30), CalendarMove::Days(1)), Some((2026, 12, 1)), "into the next month");
        assert_eq!(moved((2026, 1, 3), CalendarMove::Days(-7)), Some((2025, 12, 27)), "a week back, across the year");
        assert_eq!(moved((2026, 1, 31), CalendarMove::Months(1)), Some((2026, 2, 28)), "to February's last day");
        assert_eq!(moved((2026, 3, 15), CalendarMove::Months(-3)), Some((2025, 12, 15)));
        assert_eq!(moved((2000, 1, 1), CalendarMove::Today), Some(today()));
    }

    #[test]
    fn week_starts() {
        // The grid's first column follows the locale: Sunday in en_US,
        // Monday in en_GB (November 2026 begins on a Sunday).
        let grid_start = |locale: &str| {
            helix_i18n::with_thread_locale(libc::LC_TIME_MASK, locale, || {
                let start = locale_week_start();
                let state = CalendarState { month: (2026, 11), week_start: Some(start), ..CalendarState::default() };
                (start, state.first_shown().map(|d| d.ymd()))
            })
        };
        let Some(us) = grid_start("en_US.UTF-8") else {
            eprintln!("en_US.UTF-8 is not installed; skipping");
            return;
        };
        assert_eq!(us, (7, Some((2026, 11, 1))), "en_US: Sunday");
        let Some(gb) = grid_start("en_GB.UTF-8") else {
            eprintln!("en_GB.UTF-8 is not installed; skipping");
            return;
        };
        assert_eq!(gb, (1, Some((2026, 10, 26))), "en_GB: Monday");
    }
}
