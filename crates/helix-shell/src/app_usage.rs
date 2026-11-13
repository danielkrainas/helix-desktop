//! App usage (helix-desktop-spec `contracts/gsettings/org.gnome.desktop.privacy.md`,
//! "App usage"): with `remember-app-usage`, launches from the shell are
//! counted, and the launcher's search ranks often and recently used apps
//! first among equal matches.

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

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// `remember-app-usage` (true without the key).
pub fn enabled() -> bool {
    use gio::prelude::*;
    if !helix_settings::has_schema(SCHEMA) {
        return true;
    }
    let s = gio::Settings::new(SCHEMA);
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

/// Count a launch of `id`.
pub fn record(id: &str) {
    if !enabled() {
        let _ = std::fs::remove_file(path());
        return;
    }
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
}
