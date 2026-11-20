//! Media players and power (helixdesktop/spec
//! `contracts/agent-access/cli.md`; helixdesktop/desktop#156): `media`
//! (`media list`) and `power` (`power status`) read, `media play`,
//! `pause`, `play-pause`, `next`, `previous`, `stop` and `seek` control a
//! player. Each is one `Agent1` call.

use std::collections::HashMap;

use helix_i18n::{tr, trf};
use serde_json::Value as Json;
use zbus::zvariant::{OwnedValue, Value};

use crate::errors::Failure;
use crate::read::{get_bool, get_num, get_str, table};
use crate::{cli, json, Done};

type Record = HashMap<String, OwnedValue>;

/// Run the command at `path` (`"media seek"`).
pub fn run(p: &cli::Parsed, path: &str) -> Result<Done, Failure> {
    let timeout = p.timeout.unwrap_or(crate::DEFAULT_TIMEOUT);
    let mut options = crate::options(p);
    match path {
        "media list" => {
            let (players,): (Vec<Record>,) = crate::call_for(p, "GetMedia", &(options,), timeout)?;
            let mut out = serde_json::Map::new();
            out.insert("players".to_owned(), Json::Array(players.iter().map(json::record).collect()));
            Ok(Done { json: Json::Object(out), text: players_text(&players) })
        }
        "power status" => {
            let (power,): (Record,) = crate::call_for(p, "GetPower", &(options,), timeout)?;
            Ok(Done { json: json::record(&power), text: power_text(&power) })
        }
        _ => {
            let action = path.strip_prefix("media ").unwrap_or(path);
            if action == "seek" {
                let (key, seconds) = seek(&p.args[0])?;
                options.insert(key, Value::from(seconds));
            }
            let player = p.opt("player").unwrap_or("");
            crate::call_for::<_, ()>(p, "MediaControl", &(player, action, options), timeout)?;
            Ok(Done { json: serde_json::json!({}), text: String::new() })
        }
    }
}

/// `media seek`'s `SECONDS`: with a sign, an `offset`; without, a
/// `position`.
pub fn seek(text: &str) -> Result<(&'static str, f64), Failure> {
    let bad = || Failure::usage(trf!("“{value}” is not a number of seconds", value = text));
    let signed = text.starts_with('+') || text.starts_with('-');
    let seconds = text.trim_start_matches('+').parse::<f64>().ok().filter(|s| s.is_finite()).ok_or_else(bad)?;
    Ok(if signed { ("offset", seconds) } else { ("position", seconds) })
}

/// A player's status as people read it.
fn status_name(status: &str) -> String {
    match status {
        "playing" => tr("playing").to_owned(),
        "paused" => tr("paused").to_owned(),
        _ => tr("stopped").to_owned(),
    }
}

/// A line per player: `*` for the active one, its name, its status, and
/// “title — artist” when it tells them.
pub fn players_text(players: &[Record]) -> String {
    if players.is_empty() {
        return tr("No media players.").to_owned();
    }
    let rows: Vec<Vec<String>> = players
        .iter()
        .map(|r| {
            let active = if get_bool(r, "active") == Some(true) { "*" } else { " " };
            let artist = r.get("artist").and_then(|v| Vec::<String>::try_from(v.clone()).ok()).unwrap_or_default().join(", ");
            let track = match (get_str(r, "title"), artist.is_empty()) {
                (title, _) if title.is_empty() => artist,
                (title, true) => title,
                (title, false) => format!("{title} — {artist}"),
            };
            vec![active.to_owned(), get_str(r, "name"), get_str(r, "id"), status_name(&get_str(r, "status")), track]
        })
        .collect();
    table(&rows)
}

/// A battery's state as people read it.
fn battery_state(state: &str) -> Option<String> {
    Some(
        match state {
            "charging" => tr("charging"),
            "discharging" => tr("discharging"),
            "empty" => tr("empty"),
            "fully-charged" => tr("fully charged"),
            "pending-charge" => tr("waiting to charge"),
            "pending-discharge" => tr("waiting to discharge"),
            _ => return None,
        }
        .to_owned(),
    )
}

/// A power profile as quick settings names it.
pub fn profile_name(profile: &str) -> String {
    match profile {
        "power-saver" => tr("Power Saver").to_owned(),
        "balanced" => tr("Balanced").to_owned(),
        "performance" => tr("Performance").to_owned(),
        other => other.to_owned(),
    }
}

/// A time span in hours and minutes.
fn span(seconds: f64) -> String {
    let minutes = (seconds / 60.0).round() as u64;
    if minutes >= 60 {
        trf!("{hours} h {minutes} min", hours = minutes / 60, minutes = minutes % 60)
    } else {
        trf!("{minutes} min", minutes = minutes)
    }
}

/// The battery's percentage, state and time left, and the power mode;
/// nothing for what the computer lacks.
pub fn power_text(power: &Record) -> String {
    let mut lines = Vec::new();
    if let Some(percent) = get_num(power, "percentage") {
        let mut parts = vec![format!("{percent:.0}%")];
        if let Some(state) = battery_state(&get_str(power, "battery-state")) {
            parts.push(state);
        }
        if let Some(left) = get_num(power, "time-to-empty") {
            parts.push(trf!("{time} left", time = span(left)));
        } else if let Some(full) = get_num(power, "time-to-full") {
            parts.push(trf!("{time} until full", time = span(full)));
        }
        lines.push(trf!("Battery: {state}", state = parts.join(", ")));
    }
    let profile = get_str(power, "power-profile");
    if !profile.is_empty() {
        let others: Vec<String> = power
            .get("power-profiles")
            .and_then(|v| Vec::<String>::try_from(v.clone()).ok())
            .unwrap_or_default()
            .iter()
            .filter(|p| **p != profile)
            .map(|p| profile_name(p))
            .collect();
        lines.push(if others.is_empty() {
            trf!("Power mode: {mode}", mode = profile_name(&profile))
        } else {
            trf!("Power mode: {mode} (also: {others})", mode = profile_name(&profile), others = others.join(", "))
        });
    }
    if lines.is_empty() {
        return tr("No battery or power modes.").to_owned();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pairs: &[(&str, Value<'static>)]) -> Record {
        pairs.iter().map(|(k, v)| (k.to_string(), OwnedValue::try_from(v.clone()).unwrap())).collect()
    }

    #[test]
    fn seeking() {
        assert_eq!(seek("+30").unwrap(), ("offset", 30.0));
        assert_eq!(seek("-10.5").unwrap(), ("offset", -10.5));
        assert_eq!(seek("75").unwrap(), ("position", 75.0));
        assert_eq!(seek("soon").unwrap_err().code, "usage");
        assert_eq!(seek("inf").unwrap_err().code, "usage");
    }

    #[test]
    fn texts() {
        let vlc = record(&[
            ("id", Value::from("vlc")),
            ("name", Value::from("VLC media player")),
            ("status", Value::from("playing")),
            ("active", Value::from(true)),
            ("title", Value::from("Song")),
            ("artist", Value::from(vec!["A", "B"])),
        ]);
        let web = record(&[("id", Value::from("firefox")), ("name", Value::from("Firefox")), ("status", Value::from("paused")), ("active", Value::from(false))]);
        let text = players_text(&[vlc, web]);
        assert!(text.starts_with("*  VLC media player  vlc      playing  Song — A, B\n"), "{text}");
        assert!(text.contains("   Firefox           firefox  paused"), "{text}");
        assert_eq!(players_text(&[]), "No media players.");
        let power = record(&[
            ("percentage", Value::from(54.0)),
            ("battery-state", Value::from("discharging")),
            ("time-to-empty", Value::from(7800i64)),
            ("power-profile", Value::from("balanced")),
            ("power-profiles", Value::from(vec!["power-saver", "balanced", "performance"])),
        ]);
        assert_eq!(power_text(&power), "Battery: 54%, discharging, 2 h 10 min left\nPower mode: Balanced (also: Power Saver, Performance)");
        let modes = record(&[("power-profile", Value::from("balanced")), ("power-profiles", Value::from(vec!["balanced"]))]);
        assert_eq!(power_text(&modes), "Power mode: Balanced");
        assert_eq!(power_text(&Record::new()), "No battery or power modes.");
    }
}
