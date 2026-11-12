//! Break reminders and screen time limits (helix-desktop-spec
//! `contracts/gsettings/wellbeing.md`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use gio::prelude::*;
use tokio::sync::mpsc;
use zbus::Connection;

use crate::notifications::Center;
use crate::sound::{self, Sound};
use crate::UiEvent;

const BREAKS: &str = "org.gnome.desktop.break-reminders";
const LIMITS: &str = "org.gnome.desktop.screen-time-limits";
/// Idle for less than this counts as working (seconds).
const WORKING_IDLE: u64 = 5;
/// The upcoming notice comes this long before a break (seconds).
const UPCOMING: u64 = 60;
/// History kept, in days.
const HISTORY_DAYS: usize = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Eyesight,
    Movement,
}

impl Kind {
    pub fn key(self) -> &'static str {
        match self {
            Kind::Eyesight => "eyesight",
            Kind::Movement => "movement",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Kind::Eyesight => "Eyesight Break",
            Kind::Movement => "Movement Break",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Kind::Eyesight => "Look at something far away",
            Kind::Movement => "Stand up and move around",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BreakSettings {
    pub interval: u64,
    pub duration: u64,
    pub delay: u64,
    pub notify: bool,
    pub notify_upcoming: bool,
    pub notify_overdue: bool,
    pub countdown: bool,
    pub fade: bool,
    pub lock: bool,
    pub sound: bool,
}

/// What a tick asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    Upcoming(Kind),
    /// A break starts (`overdue`: it was delayed before).
    Start { kind: Kind, overdue: bool },
    /// Seconds of rest still needed.
    Progress { kind: Kind, left: u64 },
    End(Kind),
}

#[derive(Debug, Clone, Default)]
struct Timer {
    /// Active seconds since the last break.
    active: u64,
    /// When the break is due (active seconds); 0: at the interval.
    due_at: u64,
    delayed: bool,
    upcoming_sent: bool,
    in_break: bool,
}

/// The break reminders' state.
#[derive(Debug, Default)]
pub struct Breaks {
    timers: [Timer; 2],
    locked_for: u64,
}

impl Breaks {
    fn reset(&mut self, kind: Kind) {
        self.timers[kind.index()] = Timer::default();
    }

    pub fn in_break(&self, kind: Kind) -> bool {
        self.timers[kind.index()].in_break
    }

    /// The break to show: movement before eyesight.
    pub fn shown(&self) -> Option<Kind> {
        [Kind::Movement, Kind::Eyesight].into_iter().find(|k| self.in_break(*k))
    }

    /// One second passed; `selected` are the kinds on, with their settings.
    pub fn tick(&mut self, selected: &[(Kind, BreakSettings)], idle_secs: u64, locked: bool) -> Vec<Out> {
        self.locked_for = if locked { self.locked_for + 1 } else { 0 };
        let rest = idle_secs.max(self.locked_for);
        let working = !locked && idle_secs < WORKING_IDLE;
        let mut out = Vec::new();
        for (kind, s) in selected {
            let t = &mut self.timers[kind.index()];
            if rest >= s.duration {
                if t.in_break {
                    out.push(Out::End(*kind));
                }
                self.reset(*kind);
                // A movement break is an eyesight break too.
                if *kind == Kind::Movement {
                    if self.timers[Kind::Eyesight.index()].in_break {
                        out.push(Out::End(Kind::Eyesight));
                    }
                    self.reset(Kind::Eyesight);
                }
                continue;
            }
            if t.in_break {
                out.push(Out::Progress { kind: *kind, left: s.duration - rest });
                continue;
            }
            if working {
                t.active += 1;
            }
            let due = if t.due_at == 0 { s.interval } else { t.due_at };
            if s.notify_upcoming && !t.upcoming_sent && !t.delayed && t.active + UPCOMING >= due && t.active < due {
                t.upcoming_sent = true;
                out.push(Out::Upcoming(*kind));
            }
            if t.active >= due {
                t.in_break = true;
                out.push(Out::Start { kind: *kind, overdue: t.delayed });
            }
        }
        // Kinds turned off forget their state.
        for kind in [Kind::Eyesight, Kind::Movement] {
            if !selected.iter().any(|(k, _)| *k == kind) {
                if self.in_break(kind) {
                    out.push(Out::End(kind));
                }
                self.reset(kind);
            }
        }
        out
    }

    /// "Delay": the break comes back after `delay` more active seconds.
    pub fn delay(&mut self, kind: Kind, delay: u64) {
        let t = &mut self.timers[kind.index()];
        t.in_break = false;
        t.delayed = true;
        t.due_at = t.active + delay;
    }

    /// "Skip": as if the break was taken.
    pub fn skip(&mut self, kind: Kind) {
        self.reset(kind);
    }
}

fn break_settings(kind: Kind) -> Option<BreakSettings> {
    let id = format!("{BREAKS}.{}", kind.key());
    if !helix_settings::has_schema(&id) {
        return None;
    }
    let s = gio::Settings::new(&id);
    let secs = |k: &str| u64::from(s.uint(k));
    Some(BreakSettings {
        interval: secs("interval-seconds"),
        duration: secs("duration-seconds"),
        delay: secs("delay-seconds"),
        notify: s.boolean("notify"),
        notify_upcoming: s.boolean("notify-upcoming"),
        notify_overdue: s.boolean("notify-overdue"),
        countdown: s.boolean("countdown"),
        fade: s.boolean("fade-screen"),
        lock: s.boolean("lock-screen"),
        sound: s.boolean("play-sound"),
    })
}

/// The selected kinds and their settings.
fn selected_breaks() -> Vec<(Kind, BreakSettings)> {
    if !helix_settings::has_schema(BREAKS) {
        return Vec::new();
    }
    let selected: Vec<String> = gio::Settings::new(BREAKS).strv("selected-breaks").iter().map(|s| s.to_string()).collect();
    [Kind::Eyesight, Kind::Movement]
        .into_iter()
        .filter(|k| selected.iter().any(|s| s == k.key()))
        .filter_map(|k| Some((k, break_settings(k)?)))
        .collect()
}

// ----- Screen time -------------------------------------------------------

/// `YYYY-MM-DD seconds` lines.
pub fn parse_history(text: &str) -> BTreeMap<String, u64> {
    text.lines()
        .filter_map(|l| {
            let (day, secs) = l.trim().split_once(' ')?;
            (day.len() == 10).then(|| Some((day.to_owned(), secs.trim().parse().ok()?)))?
        })
        .collect()
}

/// The newest `HISTORY_DAYS` days, oldest first.
pub fn format_history(history: &BTreeMap<String, u64>) -> String {
    let skip = history.len().saturating_sub(HISTORY_DAYS);
    history.iter().skip(skip).map(|(d, s)| format!("{d} {s}\n")).collect()
}

/// "H hours and M minutes".
pub fn duration_text(secs: u64) -> String {
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    let plural = |n: u64, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    match (h, m) {
        (0, m) => plural(m, "minute"),
        (h, 0) => plural(h, "hour"),
        (h, m) => format!("{} and {}", plural(h, "hour"), plural(m, "minute")),
    }
}

fn history_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("helix/screen-time"))
}

fn today() -> String {
    gio::glib::DateTime::now_local().and_then(|d| d.format("%Y-%m-%d")).map(|s| s.to_string()).unwrap_or_default()
}

struct Limits {
    history: bool,
    enabled: bool,
    limit: u64,
    grayscale: bool,
}

fn limits() -> Option<Limits> {
    if !helix_settings::has_schema(LIMITS) {
        return None;
    }
    let s = gio::Settings::new(LIMITS);
    Some(Limits { history: s.boolean("history-enabled"), enabled: s.boolean("daily-limit-enabled"), limit: u64::from(s.uint("daily-limit-seconds")), grayscale: s.boolean("grayscale") })
}

/// Seconds of idleness after which time stops counting as screen time.
fn screen_idle_limit() -> u64 {
    const SESSION: &str = "org.gnome.desktop.session";
    let delay = if helix_settings::has_schema(SESSION) { u64::from(gio::Settings::new(SESSION).uint("idle-delay")) } else { 0 };
    if delay == 0 { 300 } else { delay }
}

// ----- Running -----------------------------------------------------------

/// The user's answer on the break screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Delay,
    Skip,
}

static ANSWERS: OnceLock<mpsc::UnboundedSender<Answer>> = OnceLock::new();

/// The break screen's "Delay" or "Skip".
pub fn answer(answer: Answer) {
    if let Some(tx) = ANSWERS.get() {
        let _ = tx.send(answer);
    }
}

/// What the break screen shows.
#[derive(Debug, Clone, PartialEq)]
pub struct BreakView {
    pub title: String,
    pub message: String,
    /// Seconds left, when counting down.
    pub left: Option<u64>,
}

async fn idle_ms(conn: &Connection) -> u64 {
    let reply = conn.call_method(Some("org.gnome.Mutter.IdleMonitor"), "/org/gnome/Mutter/IdleMonitor/Core", Some("org.gnome.Mutter.IdleMonitor"), "GetIdletime", &()).await;
    reply.ok().and_then(|r| r.body().deserialize::<u64>().ok()).unwrap_or(0)
}

async fn set_grayscale(conn: &Connection, on: bool) {
    let result = conn.call_method(Some("org.gnome.Mutter.DisplayConfig"), "/org/gnome/Mutter/DisplayConfig", Some("org.helix.ScreenEffects"), "SetGrayscale", &(on,)).await;
    if let Err(err) = result {
        tracing::warn!(%err, on, "cannot change grayscale");
    }
}

/// Run for the rest of the session.
pub async fn run(conn: Connection, center: Center, ui: mpsc::UnboundedSender<UiEvent>) {
    let (tx, mut answers) = mpsc::unbounded_channel();
    let _ = ANSWERS.set(tx);
    let mut breaks = Breaks::default();
    let mut view: Option<BreakView> = None;
    // Screen time.
    let path = history_path();
    let mut history = match (&path, limits().is_some_and(|l| l.history)) {
        (Some(p), true) => parse_history(&std::fs::read_to_string(p).unwrap_or_default()),
        _ => BTreeMap::new(),
    };
    let (mut day, mut notified_day, mut gray, mut ticks) = (today(), String::new(), false, 0u64);
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            Some(a) = answers.recv() => {
                if let Some(kind) = breaks.shown() {
                    tracing::info!(?a, ?kind, "break answered");
                    match a {
                        Answer::Delay => breaks.delay(kind, break_settings(kind).map_or(180, |s| s.delay)),
                        Answer::Skip => breaks.skip(kind),
                    }
                    // A shown movement break covers eyesight too.
                    if kind == Kind::Movement && breaks.in_break(Kind::Eyesight) {
                        match a {
                            Answer::Delay => breaks.delay(Kind::Eyesight, break_settings(Kind::Eyesight).map_or(180, |s| s.delay)),
                            Answer::Skip => breaks.skip(Kind::Eyesight),
                        }
                    }
                }
                view = None;
                let _ = ui.send(UiEvent::Break(None));
                continue;
            }
        }
        ticks += 1;
        let idle = idle_ms(&conn).await / 1000;
        let locked = center.locked();
        // Breaks.
        let selected = selected_breaks();
        let setting = |k: Kind| selected.iter().find(|(x, _)| *x == k).map(|(_, s)| s.clone());
        let mut left: [Option<u64>; 2] = [None, None];
        for out in breaks.tick(&selected, idle, locked) {
            tracing::debug!(?out, "break reminders");
            match out {
                Out::Upcoming(kind) => {
                    center.notice("Break Reminders", &format!("{} Soon", kind.title()), "A break starts in one minute.", "alarm-symbolic", false);
                }
                Out::Start { kind, overdue } => {
                    let Some(s) = setting(kind) else { continue };
                    if overdue && s.notify_overdue {
                        let minutes = s.interval / 60;
                        center.notice("Break Reminders", "Break Overdue", &format!("You have been working for {minutes} minutes without a break."), "alarm-symbolic", false);
                    }
                    if s.sound {
                        sound::play(&Sound::Name("alarm-clock-elapsed".into()));
                    }
                    if s.lock && !locked {
                        if let Err(err) = crate::lock_via(&conn).await {
                            tracing::warn!(%err, "cannot lock for the break");
                        }
                    }
                    if !s.fade && s.notify && !overdue {
                        center.notice("Break Reminders", kind.title(), kind.message(), "alarm-symbolic", false);
                    }
                }
                Out::Progress { kind, left: l } => left[kind.index()] = Some(l),
                Out::End(kind) => {
                    if setting(kind).is_some_and(|s| s.sound) {
                        sound::play(&Sound::Name("complete".into()));
                    }
                }
            }
        }
        // The break screen.
        let wanted = breaks.shown().and_then(|kind| {
            let s = setting(kind).filter(|s| s.fade)?;
            let left = left[kind.index()].unwrap_or(s.duration);
            Some(BreakView { title: kind.title().into(), message: kind.message().into(), left: s.countdown.then_some(left) })
        });
        if wanted != view {
            view = wanted.clone();
            let _ = ui.send(UiEvent::Break(wanted));
        }
        // Screen time.
        let now = today();
        if now != day {
            day = now;
            notified_day.clear();
        }
        let Some(l) = limits() else { continue };
        if !locked && idle < screen_idle_limit() {
            *history.entry(day.clone()).or_default() += 1;
        }
        let total = history.get(&day).copied().unwrap_or(0);
        if l.history && ticks % 60 == 0 {
            if let Some(p) = &path {
                let _ = std::fs::create_dir_all(p.parent().unwrap_or(p));
                if let Err(err) = std::fs::write(p, format_history(&history)) {
                    tracing::warn!(%err, "cannot keep the screen time history");
                }
            }
        }
        let reached = l.enabled && total >= l.limit;
        if reached && notified_day != day {
            notified_day = day.clone();
            tracing::info!(total, limit = l.limit, "screen time limit reached");
            center.notice("Screen Time", "Screen Time Limit Reached", &format!("You have used the computer for {} today.", duration_text(total)), "preferences-system-time-symbolic", false);
        }
        let want_gray = reached && l.grayscale;
        if want_gray != gray {
            gray = want_gray;
            set_grayscale(&conn, gray).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(interval: u64, duration: u64) -> BreakSettings {
        BreakSettings { interval, duration, delay: 30, notify: true, notify_upcoming: true, notify_overdue: true, countdown: true, fade: true, lock: false, sound: false }
    }

    fn run(b: &mut Breaks, sel: &[(Kind, BreakSettings)], secs: u64, idle: impl Fn(u64) -> u64) -> Vec<(u64, Out)> {
        (0..secs).flat_map(|t| b.tick(sel, idle(t), false).into_iter().map(move |o| (t, o))).filter(|(_, o)| !matches!(o, Out::Progress { .. })).collect()
    }

    #[test]
    fn eyesight_break() {
        let sel = [(Kind::Eyesight, settings(100, 20))];
        let mut b = Breaks::default();
        let outs = run(&mut b, &sel, 100, |_| 0);
        assert_eq!(outs, [(39, Out::Upcoming(Kind::Eyesight)), (99, Out::Start { kind: Kind::Eyesight, overdue: false })]);
        // Resting 20 s ends it.
        let outs = run(&mut b, &sel, 21, |t| t);
        assert_eq!(outs, [(20, Out::End(Kind::Eyesight))]);
        assert!(!b.in_break(Kind::Eyesight));
    }

    #[test]
    fn natural_rest_and_delay() {
        let sel = [(Kind::Eyesight, settings(100, 20))];
        let mut b = Breaks::default();
        run(&mut b, &sel, 50, |_| 0);
        // A 20 s pause on its own counts as the break.
        run(&mut b, &sel, 21, |t| t);
        assert!(run(&mut b, &sel, 98, |_| 0).iter().all(|(_, o)| !matches!(o, Out::Start { .. })), "the count started over");
        let mut b = Breaks::default();
        run(&mut b, &sel, 100, |_| 0);
        b.delay(Kind::Eyesight, 30);
        let outs = run(&mut b, &sel, 30, |_| 0);
        assert_eq!(outs, [(29, Out::Start { kind: Kind::Eyesight, overdue: true })]);
        b.skip(Kind::Eyesight);
        assert!(!b.in_break(Kind::Eyesight));
    }

    #[test]
    fn movement_covers_eyesight() {
        let sel = [(Kind::Eyesight, settings(100, 20)), (Kind::Movement, settings(100, 60))];
        let mut b = Breaks::default();
        run(&mut b, &sel, 100, |_| 0);
        assert_eq!(b.shown(), Some(Kind::Movement));
        let outs = run(&mut b, &sel, 61, |t| t);
        assert!(outs.contains(&(20, Out::End(Kind::Eyesight))));
        assert!(outs.contains(&(60, Out::End(Kind::Movement))));
        assert_eq!(b.shown(), None);
    }

    #[test]
    fn screen_time() {
        let h = parse_history("2026-11-10 3600\n2026-11-11 7260\nbad line\n");
        assert_eq!(h.get("2026-11-11"), Some(&7260));
        assert_eq!(h.len(), 2);
        assert_eq!(format_history(&h), "2026-11-10 3600\n2026-11-11 7260\n");
        assert_eq!(duration_text(7260), "2 hours and 1 minute");
        assert_eq!(duration_text(3600), "1 hour");
        assert_eq!(duration_text(59), "0 minutes");
    }
}
