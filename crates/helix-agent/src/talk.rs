//! The *talk to the user* commands (helixdesktop/spec
//! `contracts/agent-access/cli.md`; helixdesktop/desktop#148): `ask`,
//! `confirm` and `remind add|list|cancel`. Each is one `Agent1` call.
//! `confirm` answered no exits 20 with `{"yes": false}`; a dismissed
//! question is the error `no` (20), one left unanswered `timed-out` (21).

use std::collections::HashMap;

use helix_i18n::{tr, trf};
use serde_json::Value as Json;
use zbus::zvariant::{OwnedValue, Value};

use crate::errors::Failure;
use crate::read::{call_timeout, millis, own_timeout};
use crate::{cli, json, Done};

/// `ask`'s and `confirm`'s `--timeout` default (seconds).
pub const QUESTION_TIMEOUT: f64 = 300.0;

type Record = HashMap<String, OwnedValue>;

/// Run the command at `path` (`"remind add"`).
pub fn run(p: &cli::Parsed, path: &str) -> Result<Done, Failure> {
    let timeout = p.timeout.unwrap_or(crate::DEFAULT_TIMEOUT);
    let mut options = crate::options(p);
    match path {
        "ask" | "confirm" => {
            let wait = own_timeout(p, QUESTION_TIMEOUT)?;
            // The shell closes the question then; the CLI waits for its
            // answer that long and the global --timeout on top.
            if wait > 0.0 {
                options.insert("timeout-ms", Value::from(millis(wait)));
            }
            let question = p.args[0].as_str();
            if path == "ask" {
                let choices: Vec<String> = p.opts_all("choice").into_iter().map(str::to_owned).collect();
                if !choices.is_empty() {
                    options.insert("choices", Value::from(choices));
                }
                if let Some(default) = p.opt("default") {
                    options.insert("default", Value::from(default.to_owned()));
                }
                let (answer,): (Record,) = crate::call_for(p, "Ask", &(question, options), call_timeout(p, wait))?;
                let text = answer.get("answer").and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
                let choice = answer.get("choice").and_then(|v| i32::try_from(v.clone()).ok()).unwrap_or(-1);
                return Ok(Done { json: serde_json::json!({ "answer": text, "choice": choice }), text });
            }
            for key in ["yes", "no"] {
                if let Some(label) = p.opt(key) {
                    options.insert(key, Value::from(label.to_owned()));
                }
            }
            let (yes,): (bool,) = crate::call_for(p, "Confirm", &(question, options), call_timeout(p, wait))?;
            if yes { Ok(Done { json: serde_json::json!({ "yes": true }), text: String::new() }) } else { Err(Failure::said_no()) }
        }
        "remind add" => {
            let time = crate::when::parse(&p.args[0], crate::now()).map_err(Failure::usage)?;
            let (id,): (String,) = crate::call_for(p, "AddReminder", &(time, p.args[1].as_str(), options), timeout)?;
            let text = trf!("Reminder {id} set for {time}", id = id, time = local_time(time));
            Ok(Done { json: serde_json::json!({ "id": id }), text })
        }
        "remind list" => {
            let (reminders,): (Vec<Record>,) = crate::call_for(p, "ListReminders", &(options,), timeout)?;
            let text = if reminders.is_empty() {
                tr("No reminders.").to_owned()
            } else {
                let field = |r: &Record, k: &str| r.get(k).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
                let at = |r: &Record| r.get("time").and_then(|v| i64::try_from(v.clone()).ok()).map(local_time).unwrap_or_default();
                reminders.iter().map(|r| format!("{}  {}  {}", field(r, "id"), at(r), field(r, "text"))).collect::<Vec<_>>().join("\n")
            };
            Ok(Done { json: serde_json::json!({ "reminders": reminders.iter().map(json::record).collect::<Vec<Json>>() }), text })
        }
        "remind cancel" => {
            crate::call_for::<_, ()>(p, "CancelReminder", &(p.args[0].as_str(), options), timeout)?;
            Ok(Done { json: serde_json::json!({}), text: String::new() })
        }
        _ => Err(Failure::not_supported()),
    }
}

/// Unix seconds as local "YYYY-MM-DD HH:MM".
fn local_time(time: i64) -> String {
    gio::glib::DateTime::from_unix_local(time).ok().and_then(|d| d.format("%Y-%m-%d %H:%M").ok()).map(|s| s.to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn said_no() {
        let no = Failure::said_no();
        assert_eq!((no.exit_code(), no.output), (20, Some(serde_json::json!({ "yes": false }))));
        assert_eq!(local_time(0).len(), 16);
    }
}
