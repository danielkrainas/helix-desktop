//! The *Read only* commands (helixdesktop/spec
//! `contracts/agent-access/cli.md`; helixdesktop/desktop#146): `state`,
//! `windows`, `apps`, `workspaces`, `monitors`, `notifications`, `files
//! recent`, `files search`, `wait window` and `wait closed`. Each is one
//! `Agent1` call; JSON output is the shell's records, text output a table
//! or a few lines for people.

use std::collections::HashMap;

use helix_i18n::{tr, trf};
use serde_json::Value as Json;
use zbus::zvariant::{OwnedValue, Value};

use crate::errors::Failure;
use crate::{cli, json, when, Done};

type Record = HashMap<String, OwnedValue>;

/// `wait`'s `--timeout` default (seconds).
pub const WAIT_TIMEOUT: f64 = 60.0;

/// Run the command at `path` (`"files search"`).
pub fn run(p: &cli::Parsed, path: &str) -> Result<Done, Failure> {
    let timeout = p.timeout.unwrap_or(crate::DEFAULT_TIMEOUT);
    let mut options = crate::options(p);
    match path {
        "state" => {
            let (state,): (Record,) = crate::call_for(p, "GetState", &(options,), timeout)?;
            Ok(Done { json: json::record(&state), text: state_text(&state) })
        }
        "windows" => {
            if let Some(app) = p.opt("app") {
                options.insert("app", Value::from(app.to_owned()));
            }
            if p.has("focused") {
                options.insert("focused", Value::from(true));
            }
            let (windows,): (Vec<Record>,) = crate::call_for(p, "ListWindows", &(options,), timeout)?;
            Ok(list("windows", &windows, windows_text(&windows)))
        }
        "apps" => {
            if p.has("running") {
                options.insert("running", Value::from(true));
            }
            let (apps,): (Vec<Record>,) = crate::call_for(p, "ListApps", &(options,), timeout)?;
            Ok(list("apps", &apps, apps_text(&apps)))
        }
        "workspaces" => {
            let (spaces,): (Vec<Record>,) = crate::call_for(p, "ListWorkspaces", &(options,), timeout)?;
            Ok(list("workspaces", &spaces, workspaces_text(&spaces)))
        }
        "monitors" => {
            let (monitors,): (Vec<Record>,) = crate::call_for(p, "ListMonitors", &(options,), timeout)?;
            Ok(list("monitors", &monitors, monitors_text(&monitors)))
        }
        "notifications" => {
            if let Some(n) = limit(p)? {
                options.insert("limit", Value::from(n));
            }
            let (notes,): (Vec<Record>,) = crate::call_for(p, "ListNotifications", &(options,), timeout)?;
            Ok(list("notifications", &notes, notifications_text(&notes)))
        }
        "files recent" | "files search" => {
            if let Some(n) = limit(p)? {
                options.insert("limit", Value::from(n));
            }
            if let Some(mime) = p.opt("type") {
                options.insert("type", Value::from(mime.to_owned()));
            }
            let (files,): (Vec<Record>,) = if path == "files recent" {
                crate::call_for(p, "RecentFiles", &(options,), timeout)?
            } else {
                let now = crate::now();
                for key in ["since", "until"] {
                    if let Some(w) = p.opt(key) {
                        options.insert(key, Value::from(when::parse(w, now).map_err(Failure::usage)?));
                    }
                }
                if let Some(folder) = p.opt("in") {
                    options.insert("in", Value::from(absolute(folder)?));
                }
                crate::call_for(p, "SearchFiles", &(p.args[0].as_str(), options), timeout)?
            };
            Ok(list("files", &files, files_text(&files)))
        }
        "wait window" => {
            let wait = wait_timeout(p)?;
            if let Some(app) = p.opt("app") {
                options.insert("app", Value::from(app.to_owned()));
            }
            if let Some(title) = p.opt("title") {
                options.insert("title", Value::from(title.to_owned()));
            }
            if p.has("focused") {
                options.insert("focused", Value::from(true));
            }
            let (window,): (Record,) = crate::call_for(p, "WaitForWindow", &(options, millis(wait)), call_timeout(p, wait))?;
            Ok(Done { json: json::record(&window), text: windows_text(std::slice::from_ref(&window)) })
        }
        "wait closed" => {
            let wait = wait_timeout(p)?;
            let window = window_id(&p.args[0])?;
            crate::call_for::<_, ()>(p, "WaitForWindowClosed", &(window, millis(wait)), call_timeout(p, wait))?;
            Ok(Done { json: serde_json::json!({}), text: String::new() })
        }
        _ => Err(Failure::not_supported()),
    }
}

/// `{"<key>": [records]}`.
fn list(key: &str, records: &[Record], text: String) -> Done {
    let mut out = serde_json::Map::new();
    out.insert(key.to_owned(), Json::Array(records.iter().map(json::record).collect()));
    Done { json: Json::Object(out), text }
}

/// `--limit`: a whole number from 1.
fn limit(p: &cli::Parsed) -> Result<Option<u32>, Failure> {
    p.opt("limit")
        .map(|n| n.parse::<u32>().ok().filter(|n| *n > 0).ok_or_else(|| Failure::usage(trf!("“{value}” is not a number from 1", value = n))))
        .transpose()
}

/// A window id from `windows`.
pub(crate) fn window_id(text: &str) -> Result<u32, Failure> {
    text.parse::<u32>().ok().filter(|n| *n > 0).ok_or_else(|| Failure::usage(trf!("“{value}” is not a window id", value = text)))
}

/// `wait`'s own `--timeout` (seconds; 0: no limit).
fn wait_timeout(p: &cli::Parsed) -> Result<f64, Failure> {
    p.opt("timeout").map(cli::seconds).transpose().map_err(Failure::usage).map(|t| t.unwrap_or(WAIT_TIMEOUT))
}

fn millis(seconds: f64) -> u32 {
    (seconds * 1000.0).round().min(f64::from(u32::MAX)) as u32
}

/// How long to wait for the shell's answer to a wait: the wait, and the
/// global `--timeout` on top (no limit when either has none).
fn call_timeout(p: &cli::Parsed, wait: f64) -> f64 {
    let answer = p.timeout.unwrap_or(crate::DEFAULT_TIMEOUT);
    if wait == 0.0 || answer == 0.0 { 0.0 } else { wait + answer }
}

/// A folder for `--in`, made absolute (`~` is the home folder).
pub(crate) fn absolute(folder: &str) -> Result<String, Failure> {
    if folder.contains("://") {
        return Ok(folder.to_owned());
    }
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let path = if folder == "~" {
        home
    } else if let Some(rest) = folder.strip_prefix("~/") {
        home.join(rest)
    } else {
        std::env::current_dir().map_err(|e| Failure::new("failed", e.to_string()))?.join(folder)
    };
    Ok(path.to_string_lossy().into_owned())
}

fn get_str(r: &Record, key: &str) -> String {
    r.get(key).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default()
}

fn get_bool(r: &Record, key: &str) -> Option<bool> {
    r.get(key).and_then(|v| bool::try_from(v.clone()).ok())
}

fn get_num(r: &Record, key: &str) -> Option<f64> {
    match &**r.get(key)? {
        Value::U32(n) => Some(f64::from(*n)),
        Value::I32(n) => Some(f64::from(*n)),
        Value::I64(n) => Some(*n as f64),
        Value::U64(n) => Some(*n as f64),
        Value::F64(n) => Some(*n),
        _ => None,
    }
}

fn int(r: &Record, key: &str) -> String {
    get_num(r, key).map(|n| format!("{n:.0}")).unwrap_or_default()
}

fn on_off(on: bool) -> String {
    if on { tr("on").to_owned() } else { tr("off").to_owned() }
}

/// A local date and time.
fn local_time(unix: f64) -> String {
    gio::glib::DateTime::from_unix_local(unix as i64).ok().and_then(|d| d.format("%Y-%m-%d %H:%M").ok()).map(|s| s.to_string()).unwrap_or_default()
}

/// Columns padded to their widest cell, the first row the headings.
pub fn table(rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns).map(|c| rows.iter().filter_map(|r| r.get(c)).map(|s| s.chars().count()).max().unwrap_or(0)).collect();
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (c, cell) in row.iter().enumerate() {
            if c + 1 == row.len() {
                line.push_str(cell);
            } else {
                line.push_str(cell);
                line.push_str(&" ".repeat(widths[c] - cell.chars().count() + 2));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn state_text(s: &Record) -> String {
    let mut lines = Vec::new();
    match get_num(s, "focused-window") {
        Some(id) if id > 0.0 => lines.push(trf!("Focused window: {id}", id = format!("{id:.0}"))),
        _ => lines.push(tr("Focused window: none").to_owned()),
    }
    lines.push(trf!("Active workspace: {index}", index = int(s, "active-workspace")));
    for (key, label) in [
        ("dark-style", tr("Dark style")),
        ("do-not-disturb", tr("Do not disturb")),
        ("night-light", tr("Night Light")),
        ("wifi", tr("Wi-Fi")),
        ("airplane-mode", tr("Airplane mode")),
    ] {
        if let Some(on) = get_bool(s, key) {
            lines.push(format!("{label}: {}", on_off(on)));
        }
    }
    if let Some(volume) = get_num(s, "volume") {
        let percent = format!("{:.0}%", volume * 100.0);
        lines.push(if get_bool(s, "muted") == Some(true) { trf!("Volume: {percent} (muted)", percent = percent) } else { trf!("Volume: {percent}", percent = percent) });
    }
    if let Some(brightness) = get_num(s, "brightness") {
        lines.push(trf!("Brightness: {percent}", percent = format!("{:.0}%", brightness * 100.0)));
    }
    lines.join("\n")
}

/// The name of a window's state as people read it.
fn state_name(state: &str) -> String {
    match state {
        "maximized" => tr("maximized").to_owned(),
        "fullscreen" => tr("full screen").to_owned(),
        "minimized" => tr("minimized").to_owned(),
        "tiled-left" => tr("left half").to_owned(),
        "tiled-right" => tr("right half").to_owned(),
        _ => tr("normal").to_owned(),
    }
}

fn windows_text(windows: &[Record]) -> String {
    if windows.is_empty() {
        return tr("No windows.").to_owned();
    }
    let mut rows = vec![vec![tr("ID").to_owned(), tr("App").to_owned(), tr("Workspace").to_owned(), tr("State").to_owned(), tr("Title").to_owned()]];
    for w in windows {
        let mut title = get_str(w, "title");
        if get_bool(w, "focused") == Some(true) {
            title = trf!("{title} (focused)", title = title);
        }
        let workspace = if get_bool(w, "all-workspaces") == Some(true) { tr("all").to_owned() } else { int(w, "workspace") };
        rows.push(vec![int(w, "id"), get_str(w, "app-id"), workspace, state_name(&get_str(w, "state")), title]);
    }
    let mut out = table(&rows);
    for w in windows.iter().filter(|w| !get_str(w, "document").is_empty()) {
        out.push_str(&trf!("Window {id} shows {document}", id = int(w, "id"), document = get_str(w, "document")));
        out.push('\n');
    }
    out
}

fn apps_text(apps: &[Record]) -> String {
    if apps.is_empty() {
        return tr("No apps.").to_owned();
    }
    let mut rows = vec![vec![tr("Name").to_owned(), tr("ID").to_owned(), tr("Windows").to_owned()]];
    rows.extend(apps.iter().map(|a| vec![get_str(a, "name"), get_str(a, "id"), int(a, "windows")]));
    table(&rows)
}

fn workspaces_text(spaces: &[Record]) -> String {
    let mut rows = vec![vec![tr("Workspace").to_owned(), tr("Windows").to_owned(), String::new()]];
    rows.extend(spaces.iter().map(|w| vec![int(w, "index"), int(w, "windows"), if get_bool(w, "active") == Some(true) { tr("active").to_owned() } else { String::new() }]));
    table(&rows)
}

fn monitors_text(monitors: &[Record]) -> String {
    if monitors.is_empty() {
        return tr("No monitors.").to_owned();
    }
    let mut rows = vec![vec![tr("Monitor").to_owned(), tr("Name").to_owned(), tr("Area").to_owned(), tr("Scale").to_owned(), String::new()]];
    for m in monitors {
        let area = format!("{}×{} @ {},{}", int(m, "width"), int(m, "height"), int(m, "x"), int(m, "y"));
        let scale = get_num(m, "scale").map(|s| format!("{s}")).unwrap_or_default();
        let primary = if get_bool(m, "primary") == Some(true) { tr("primary").to_owned() } else { String::new() };
        rows.push(vec![get_str(m, "connector"), get_str(m, "name"), area, scale, primary]);
    }
    table(&rows)
}

fn notifications_text(notes: &[Record]) -> String {
    if notes.is_empty() {
        return tr("No notifications.").to_owned();
    }
    let mut out = String::new();
    let mut redacted = false;
    for n in notes {
        let time = get_num(n, "time").map(local_time).unwrap_or_default();
        let app = get_str(n, "app-name");
        if get_bool(n, "redacted") == Some(true) {
            redacted = true;
            out.push_str(&format!("{time}  {app}\n"));
        } else {
            let body = get_str(n, "body").replace('\n', " ");
            let summary = get_str(n, "summary");
            out.push_str(&if body.is_empty() { format!("{time}  {app}: {summary}\n") } else { format!("{time}  {app}: {summary} — {body}\n") });
        }
    }
    if redacted {
        out.push_str(tr("Notification contents are hidden: Notifications access is turned off (Settings › Privacy & Security › Agent Access)."));
        out.push('\n');
    }
    out
}

fn files_text(files: &[Record]) -> String {
    if files.is_empty() {
        return tr("No files.").to_owned();
    }
    let mut rows = vec![vec![tr("Modified").to_owned(), tr("Type").to_owned(), tr("Location").to_owned()]];
    for f in files {
        let uri = get_str(f, "uri");
        let shown = uri.strip_prefix("file://").map(|p| gio::glib::Uri::unescape_string(p, None::<&str>).map_or_else(|| p.to_owned(), |s| s.to_string())).unwrap_or(uri);
        rows.push(vec![get_num(f, "modified").map(local_time).unwrap_or_default(), get_str(f, "mime-type"), shown]);
    }
    table(&rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pairs: &[(&str, Value<'static>)]) -> Record {
        pairs.iter().map(|(k, v)| (k.to_string(), OwnedValue::try_from(v.clone()).unwrap())).collect()
    }

    #[test]
    fn tables() {
        let t = table(&[vec!["ID".into(), "Title".into()], vec!["12".into(), "Notes".into()], vec!["3".into(), "".into()]]);
        assert_eq!(t, "ID  Title\n12  Notes\n3\n");
    }

    #[test]
    fn texts() {
        let w = record(&[
            ("id", Value::from(12u32)),
            ("app-id", Value::from("org.gnome.TextEditor")),
            ("title", Value::from("notes.txt")),
            ("workspace", Value::from(0u32)),
            ("all-workspaces", Value::from(false)),
            ("focused", Value::from(true)),
            ("state", Value::from("maximized")),
            ("document", Value::from("file:///tmp/notes.txt")),
        ]);
        let text = windows_text(&[w]);
        assert!(text.contains("org.gnome.TextEditor  0          maximized  notes.txt (focused)"), "{text}");
        assert!(text.contains("Window 12 shows file:///tmp/notes.txt"));
        assert_eq!(windows_text(&[]), "No windows.");
        let s = record(&[("focused-window", Value::from(0u32)), ("active-workspace", Value::from(1u32)), ("dark-style", Value::from(true)), ("volume", Value::from(0.4)), ("muted", Value::from(true))]);
        let text = state_text(&s);
        assert!(text.contains("Focused window: none") && text.contains("Active workspace: 1") && text.contains("Dark style: on") && text.contains("Volume: 40% (muted)"), "{text}");
        assert!(!text.contains("Wi-Fi"), "absent without NetworkManager");
        let n = record(&[("id", Value::from(1u32)), ("app-name", Value::from("Calendar")), ("time", Value::from(0i64)), ("redacted", Value::from(true))]);
        assert!(notifications_text(&[n]).contains("Notification contents are hidden"));
    }

    #[test]
    fn arguments() {
        assert_eq!(millis(1.5), 1500);
        assert_eq!(window_id("7").unwrap(), 7);
        assert_eq!(window_id("0").unwrap_err().code, "usage");
        assert_eq!(window_id("x").unwrap_err().code, "usage");
        assert!(absolute("/tmp").unwrap() == "/tmp");
        assert!(absolute("rel").unwrap().ends_with("/rel"));
    }
}
