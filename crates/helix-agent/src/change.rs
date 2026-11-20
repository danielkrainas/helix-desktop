//! The commands that change settings and act on notifications and files
//! (helixdesktop/spec `contracts/agent-access/cli.md`;
//! helixdesktop/desktop#157): `wallpaper`, `accent`, `power mode`,
//! `files star`, `unstar` and `reveal`, `dock pin` and `unpin`,
//! `notifications dismiss` and `press`. Those that change a setting print
//! the shell's Change record; `--until` puts the old value back then.

use std::collections::HashMap;

use helix_i18n::{tr, trf};
use zbus::zvariant::{OwnedValue, Value};

use crate::errors::Failure;
use crate::read::{get_bool, get_num, get_str};
use crate::{cli, json, when, Done};

type Record = HashMap<String, OwnedValue>;

/// Run the command at `path` (`"power mode"`).
pub fn run(p: &cli::Parsed, path: &str) -> Result<Done, Failure> {
    let timeout = p.timeout.unwrap_or(crate::DEFAULT_TIMEOUT);
    let mut options = crate::options(p);
    let done = || Ok(Done { json: serde_json::json!({}), text: String::new() });
    match path {
        "wallpaper" => {
            for key in ["variant", "fit"] {
                if let Some(v) = p.opt(key) {
                    options.insert(key, Value::from(v.to_owned()));
                }
            }
            let file = crate::read::absolute(&p.args[0])?;
            add_until(p, &mut options)?;
            let (record,): (Record,) = crate::call_for(p, "SetWallpaper", &(file.as_str(), options), timeout)?;
            Ok(changed(&record))
        }
        "accent" => {
            add_until(p, &mut options)?;
            let (record,): (Record,) = crate::call_for(p, "SetAccent", &(p.args[0].as_str(), options), timeout)?;
            Ok(changed(&record))
        }
        "power mode" => {
            add_until(p, &mut options)?;
            let (record,): (Record,) = crate::call_for(p, "SetPowerMode", &(p.args[0].as_str(), options), timeout)?;
            Ok(changed(&record))
        }
        "files star" | "files unstar" => {
            let file = crate::read::absolute(&p.args[0])?;
            let (record,): (Record,) = crate::call_for(p, "StarFile", &(file.as_str(), path == "files star", options), timeout)?;
            Ok(changed(&record))
        }
        "files reveal" => {
            let file = crate::read::absolute(&p.args[0])?;
            crate::call_for::<_, ()>(p, "RevealFile", &(file.as_str(), options), timeout)?;
            done()
        }
        "dock pin" | "dock unpin" => {
            let (record,): (Record,) = crate::call_for(p, "SetFavorite", &(p.args[0].as_str(), path == "dock pin", options), timeout)?;
            Ok(changed(&record))
        }
        "notifications dismiss" => {
            crate::call_for::<_, ()>(p, "DismissNotification", &(note_id(&p.args[0])?, options), timeout)?;
            done()
        }
        "notifications press" => {
            let action = p.args.get(1).map_or("default", String::as_str);
            crate::call_for::<_, ()>(p, "NotificationAction", &(note_id(&p.args[0])?, action, options), timeout)?;
            done()
        }
        _ => Err(Failure::not_supported()),
    }
}

/// A notification id from `notifications`.
fn note_id(text: &str) -> Result<u32, Failure> {
    text.parse::<u32>().map_err(|_| Failure::usage(trf!("“{value}” is not a notification id", value = text)))
}

/// `--until` as the option `until` (Unix seconds), checked here first
/// (10 seconds to 24 hours ahead) and only for a shell that has it, so a
/// change never silently lasts.
pub fn add_until(p: &cli::Parsed, options: &mut HashMap<&'static str, Value<'static>>) -> Result<Option<i64>, Failure> {
    let Some(at) = until(p)? else { return Ok(None) };
    options.insert("until", Value::from(at));
    Ok(Some(at))
}

fn until(p: &cli::Parsed) -> Result<Option<i64>, Failure> {
    let Some(text) = p.opt("until") else { return Ok(None) };
    let now = crate::now();
    let at = when::parse(text, now).map_err(Failure::usage)?;
    if !when::until_in_range(at, now) {
        return Err(Failure::usage(tr("“--until” must be 10 seconds to 24 hours ahead").to_owned()));
    }
    if !crate::shell_has(p, "SetWallpaper")? {
        return Err(Failure::not_supported());
    }
    Ok(Some(at))
}

/// ", back at 15:30" for a change put back at `until`.
pub fn back_at(until: i64) -> String {
    let time = gio::glib::DateTime::from_unix_local(until).ok().and_then(|d| d.format("%H:%M").ok()).map(|s| s.to_string()).unwrap_or_default();
    trf!(", back at {time}", time = time)
}

/// A Change record's setting as Settings names it.
fn setting_label(record: &Record) -> String {
    let target = get_str(record, "target");
    match get_str(record, "name").as_str() {
        "wallpaper" => tr("Background").to_owned(),
        "accent" => tr("Accent color").to_owned(),
        "power-mode" => tr("Power mode").to_owned(),
        "star" | "favorite" => target.rsplit('/').next().map(|n| gio::glib::Uri::unescape_string(n, None::<&str>).map_or_else(|| n.to_owned(), |s| s.to_string())).unwrap_or(target),
        other => other.to_owned(),
    }
}

/// A Change record's `old` or `new` value as Settings shows it.
fn value_label(record: &Record, key: &str) -> String {
    let name = get_str(record, "name");
    if let Some(on) = get_bool(record, key) {
        return match (name.as_str(), on) {
            ("star", true) => tr("starred").to_owned(),
            ("star", false) => tr("not starred").to_owned(),
            ("favorite", true) => tr("in the dock").to_owned(),
            ("favorite", false) => tr("not in the dock").to_owned(),
            (_, true) => tr("on").to_owned(),
            (_, false) => tr("off").to_owned(),
        };
    }
    let text = get_str(record, key);
    match name.as_str() {
        "accent" => accent_name(&text),
        "power-mode" => crate::media::profile_name(&text),
        "wallpaper" if text.is_empty() => tr("none").to_owned(),
        // A picture by its file's name.
        "wallpaper" => text.rsplit('/').next().map(|n| gio::glib::Uri::unescape_string(n, None::<&str>).map_or_else(|| n.to_owned(), |s| s.to_string())).unwrap_or(text),
        _ => text,
    }
}

/// An accent color as Settings names it.
fn accent_name(name: &str) -> String {
    match name {
        "blue" => tr("Blue"),
        "teal" => tr("Teal"),
        "green" => tr("Green"),
        "yellow" => tr("Yellow"),
        "orange" => tr("Orange"),
        "red" => tr("Red"),
        "pink" => tr("Pink"),
        "purple" => tr("Purple"),
        "slate" => tr("Slate"),
        other => other,
    }
    .to_owned()
}

/// A Change record's output: the record, and "Accent color: Blue → Red"
/// (", back at 15:30"), or "Accent color is already Red".
pub fn changed(record: &Record) -> Done {
    Done { json: json::record(record), text: change_text(record) }
}

fn change_text(record: &Record) -> String {
    let what = setting_label(record);
    let new = value_label(record, "new");
    if get_bool(record, "changed") == Some(false) {
        return trf!("{setting} is already {value}", setting = what, value = new);
    }
    let mut text = trf!("{setting}: {old} → {new}", setting = what, old = value_label(record, "old"), new = new);
    if let Some(until) = get_num(record, "until") {
        text.push_str(&back_at(until as i64));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pairs: &[(&str, Value<'_>)]) -> Record {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), OwnedValue::try_from(v.clone()).unwrap())).collect()
    }

    #[test]
    fn change_texts() {
        let accent = record(&[("changed", Value::from(true)), ("name", Value::from("accent")), ("old", Value::from("blue")), ("new", Value::from("red"))]);
        assert_eq!(change_text(&accent), "Accent color: Blue → Red");
        let same = record(&[("changed", Value::from(false)), ("name", Value::from("accent")), ("old", Value::from("red")), ("new", Value::from("red"))]);
        assert_eq!(change_text(&same), "Accent color is already Red");
        let at = gio::glib::DateTime::from_local(2026, 11, 20, 15, 30, 0.0).unwrap().to_unix();
        let mode = record(&[("changed", Value::from(true)), ("name", Value::from("power-mode")), ("old", Value::from("balanced")), ("new", Value::from("performance")), ("until", Value::from(at))]);
        assert_eq!(change_text(&mode), "Power mode: Balanced → Performance, back at 15:30");
        let star = record(&[("changed", Value::from(true)), ("name", Value::from("star")), ("target", Value::from("file:///home/me/My%20Notes.txt")), ("old", Value::from(false)), ("new", Value::from(true))]);
        assert_eq!(change_text(&star), "My Notes.txt: not starred → starred");
        let pin = record(&[("changed", Value::from(false)), ("name", Value::from("favorite")), ("target", Value::from("org.gnome.Maps")), ("old", Value::from(true)), ("new", Value::from(true))]);
        assert_eq!(change_text(&pin), "org.gnome.Maps is already in the dock");
        let bg = record(&[("changed", Value::from(true)), ("name", Value::from("wallpaper")), ("old", Value::from("")), ("new", Value::from("file:///home/me/.local/share/backgrounds/2026-11-20-10-00-00-sea.png"))]);
        assert_eq!(change_text(&bg), "Background: none → 2026-11-20-10-00-00-sea.png");
        assert!(note_id("x").is_err() && note_id("4").is_ok());
    }
}
