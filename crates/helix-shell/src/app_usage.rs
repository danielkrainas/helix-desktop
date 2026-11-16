//! App usage (helixdesktop/spec `contracts/gsettings/org.gnome.desktop.privacy.md`,
//! "App usage"): with `remember-app-usage`, launches from the shell are
//! counted, and the launcher's search ranks often and recently used apps
//! first among equal matches. The app grid's "Recent" row shows the apps
//! launched last ([`recent`], kept apart from the counts).

use std::collections::HashMap;
use std::path::PathBuf;

const SCHEMA: &str = "org.gnome.desktop.privacy";
/// After this many days, a launch counts half.
const HALF_LIFE_DAYS: f64 = 30.0;

/// Launches by desktop file id: (count, last launch, Unix seconds).
pub type Usage = HashMap<String, (u32, i64)>;

fn path() -> PathBuf {
    gio::glib::user_data_dir().join("helix").join("app-usage")
}

/// The apps launched last, one desktop file id a line, the latest first.
fn recent_path() -> PathBuf {
    gio::glib::user_data_dir().join("helix").join("recent-apps")
}

/// How many recent launches are kept: more than the widest grid row
/// (8), so that apps since removed or hidden can be skipped.
pub const RECENT_KEPT: usize = 16;

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// `remember-app-usage` (true without the key).
pub fn enabled() -> bool {
    if !helix_settings::has_schema(SCHEMA) {
        return true;
    }
    let s = helix_settings::Settings::new(SCHEMA);
    !s.settings_schema().is_some_and(|x| x.has_key("remember-app-usage")) || s.boolean("remember-app-usage")
}

/// The file's lines: `<id>\t<count>\t<last>`.
pub fn parse(text: &str) -> Usage {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let id = f.next()?.to_owned();
            Some((id, (f.next()?.parse().ok()?, f.next()?.parse().ok()?)))
        })
        .collect()
}

fn format(usage: &Usage) -> String {
    let mut lines: Vec<String> = usage.iter().map(|(id, (n, t))| format!("{id}\t{n}\t{t}")).collect();
    lines.sort();
    lines.join("\n") + "\n"
}

/// The usage recorded (none, and the record deleted, when usage is not
/// remembered).
pub fn read() -> Usage {
    if !enabled() {
        let _ = std::fs::remove_file(path());
        return Usage::new();
    }
    std::fs::read_to_string(path()).map(|t| parse(&t)).unwrap_or_default()
}

/// Count a launch of `id` (and make it the latest recent app).
pub fn record(id: &str) {
    if !enabled() {
        let _ = std::fs::remove_file(path());
        let _ = std::fs::remove_file(recent_path());
        return;
    }
    record_recent(id);
    let mut usage = read();
    let entry = usage.entry(id.to_owned()).or_insert((0, 0));
    entry.0 = entry.0.saturating_add(1);
    entry.1 = now();
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, format(&usage));
}

/// `recent` (the latest first) after a launch of `id`: it moves to the
/// front, once, and at most [`RECENT_KEPT`] are kept.
pub fn push_recent(recent: &[String], id: &str) -> Vec<String> {
    let mut out = vec![id.to_owned()];
    out.extend(recent.iter().filter(|r| *r != id).cloned());
    out.truncate(RECENT_KEPT);
    out
}

/// The recent apps file's lines (blank ones and repeats dropped).
pub fn parse_recent(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !out.iter().any(|r| r == id) {
            out.push(id.to_owned());
        }
    }
    out.truncate(RECENT_KEPT);
    out
}

fn record_recent(id: &str) {
    let recent = push_recent(&recent(), id);
    let p = recent_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, recent.join("\n") + "\n");
}

/// The apps launched last, the latest first (none, and the record
/// deleted, when usage is not remembered).
pub fn recent() -> Vec<String> {
    if !enabled() {
        let _ = std::fs::remove_file(recent_path());
        return Vec::new();
    }
    std::fs::read_to_string(recent_path()).map(|t| parse_recent(&t)).unwrap_or_default()
}

/// Forget the recent apps ("Clear Recent Apps"); the launch counts that
/// rank search results stay.
pub fn clear_recent() {
    let _ = std::fs::remove_file(recent_path());
}

/// Whether the app grid shows its "Recent" row: `org.helixdesktop.shell
/// app-grid-recent` (true without the key) and `remember-app-usage`.
pub fn recent_row_on() -> bool {
    enabled() && helix_settings::Schema::open(HELIX_SCHEMA).get_or("app-grid-recent", true)
}

const HELIX_SCHEMA: &str = "org.helixdesktop.shell";

/// Tell the shell UI whether the "Recent" row shows, now and whenever
/// `app-grid-recent` or `remember-app-usage` changes.
pub async fn follow_recent_row(ui: tokio::sync::mpsc::UnboundedSender<crate::UiEvent>) {
    let _ = ui.send(crate::UiEvent::RecentApps(recent_row_on()));
    let schemas: Vec<&str> = [HELIX_SCHEMA, SCHEMA].into_iter().filter(|s| helix_settings::has_schema(s)).collect();
    if schemas.is_empty() {
        return;
    }
    let settings = helix_settings::GSettings::new(&schemas);
    let (mut changes, _watch) = settings.watch();
    let mut last = recent_row_on();
    while changes.recv().await.is_some() {
        let on = recent_row_on();
        if on != last {
            tracing::info!(on, "the app grid's recent row setting changed");
            last = on;
            let _ = ui.send(crate::UiEvent::RecentApps(on));
        }
    }
}

/// How much `id` was used: its launches, each halving in weight every
/// [`HALF_LIFE_DAYS`] since the last one.
pub fn score(usage: &Usage, id: &str, at: i64) -> f64 {
    let Some((n, last)) = usage.get(id) else { return 0.0 };
    let days = ((at - last).max(0) as f64) / 86_400.0;
    f64::from(*n) * 0.5f64.powf(days / HALF_LIFE_DAYS)
}

/// Now, for [`score`].
pub fn at() -> i64 {
    now()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores() {
        let u = parse("a.desktop\t10\t1000000\nb.desktop\t10\t0\nbad line\n");
        assert_eq!(u.len(), 2);
        let at = 1_000_000;
        assert!((score(&u, "a.desktop", at) - 10.0).abs() < 1e-9);
        assert!(score(&u, "b.desktop", at) < 10.0, "older counts less");
        assert_eq!(score(&u, "c.desktop", at), 0.0);
        assert_eq!(parse(&format(&u)), u);
    }

    #[test]
    fn recent_launches() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let r = push_recent(&[], "a.desktop");
        let r = push_recent(&r, "b.desktop");
        assert_eq!(r, ids(&["b.desktop", "a.desktop"]), "the latest first");
        let r = push_recent(&r, "a.desktop");
        assert_eq!(r, ids(&["a.desktop", "b.desktop"]), "launched again: moved to the front, once");
        let many: Vec<String> = (0..40).map(|i| format!("{i}.desktop")).collect();
        let r = many.iter().fold(Vec::new(), |r, id| push_recent(&r, id));
        assert_eq!(r.len(), RECENT_KEPT);
        assert_eq!(r[0], "39.desktop");
        assert_eq!(parse_recent("b.desktop\n\na.desktop\nb.desktop\n"), ids(&["b.desktop", "a.desktop"]));
    }
}
