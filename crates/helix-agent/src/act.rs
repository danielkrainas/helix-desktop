//! The *Control the desktop* commands (helixdesktop/spec
//! `contracts/agent-access/cli.md`; helixdesktop/desktop#147): `open`,
//! `launch`, `window`, `workspace switch`, `toggle`, `set` and `notify`.
//! Each is one `Agent1` call (`toggle` without a state reads `GetState`
//! first); JSON output is `{}` but for `toggle` and `notify`. `toggle
//! --until` and `window … to-monitor` come with the scoped actions
//! (helixdesktop/desktop#157).

use std::collections::HashMap;

use helix_i18n::trf;
use zbus::zvariant::{OwnedValue, Value};

use crate::errors::Failure;
use crate::{cli, Done};

/// Run the command at `path` (`"workspace switch"`).
pub fn run(p: &cli::Parsed, path: &str) -> Result<Done, Failure> {
    let timeout = p.timeout.unwrap_or(crate::DEFAULT_TIMEOUT);
    let mut options = crate::options(p);
    let done = || Ok(Done { json: serde_json::json!({}), text: String::new() });
    match path {
        "open" => {
            if let Some(app) = p.opt("app") {
                options.insert("app", Value::from(app.to_owned()));
            }
            let uri = uri_arg(&p.args[0])?;
            crate::call_for::<_, ()>(p, "Open", &(uri.as_str(), options), timeout)?;
            done()
        }
        "launch" => {
            let uris = p.args[1..].iter().map(|a| uri_arg(a)).collect::<Result<Vec<_>, _>>()?;
            crate::call_for::<_, ()>(p, "Launch", &(p.args[0].as_str(), uris, options), timeout)?;
            done()
        }
        "window" => {
            let action = p.args[0].as_str();
            let window = crate::read::window_id(&p.args[1])?;
            let numbers = &p.args[2..];
            let names: &[&str] = match action {
                "move" => &["x", "y"],
                "resize" => &["width", "height"],
                "to-workspace" => &["workspace"],
                "to-monitor" => &["monitor"],
                _ => &[],
            };
            if numbers.len() != names.len() {
                return Err(Failure::usage(match names.len() {
                    0 => trf!("“{action}” takes no numbers", action = action),
                    _ if action == "to-monitor" => trf!("“{action}” takes a monitor: CONNECTOR (see “helix-agent monitors”)", action = action),
                    n => trf!("“{action}” takes {n} numbers: {names}", action = action, n = n, names = names.join(" ").to_uppercase()),
                }));
            }
            for (name, text) in names.iter().zip(numbers) {
                let value = if *name == "monitor" {
                    Some(Value::from(text.clone()))
                } else if *name == "workspace" {
                    text.parse::<u32>().map(Value::from).ok()
                } else if matches!(*name, "width" | "height") {
                    text.parse::<i32>().ok().filter(|n| *n > 0).map(Value::from)
                } else {
                    text.parse::<i32>().map(Value::from).ok()
                };
                let value = value.ok_or_else(|| Failure::usage(trf!("“{value}” is not a number for {name}", value = text, name = name.to_uppercase())))?;
                options.insert(name, value);
            }
            crate::call_for::<_, ()>(p, "WindowAction", &(window, action, options), timeout)?;
            done()
        }
        "workspace switch" => {
            let index = p.args[0].parse::<u32>().map_err(|_| Failure::usage(trf!("“{value}” is not a workspace number", value = p.args[0])))?;
            crate::call_for::<_, ()>(p, "SwitchWorkspace", &(index, options), timeout)?;
            done()
        }
        "toggle" => {
            let name = p.args[0].as_str();
            let on = match p.args.get(1).map(String::as_str) {
                Some("on") => true,
                Some(_) => false,
                // Flip it: what is it now?
                None => !current(p, name, timeout)?,
            };
            let until = crate::change::add_until(p, &mut options)?;
            crate::call_for::<_, ()>(p, "SetToggle", &(name, on, options), timeout)?;
            let state = if on { helix_i18n::tr("on") } else { helix_i18n::tr("off") };
            let mut json = serde_json::json!({ "name": name, "on": on });
            let mut text = format!("{name}: {state}");
            if let Some(at) = until {
                json["until"] = serde_json::json!(at);
                text.push_str(&crate::change::back_at(at));
            }
            Ok(Done { json, text })
        }
        "set" => {
            let percent = p.args[1].trim_end_matches('%').parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0);
            let percent = percent.ok_or_else(|| Failure::usage(trf!("“{value}” is not a percentage", value = p.args[1])))?;
            crate::call_for::<_, ()>(p, "SetValue", &(p.args[0].as_str(), percent / 100.0, options), timeout)?;
            done()
        }
        "notify" => {
            if let Some(icon) = p.opt("icon") {
                options.insert("icon", Value::from(icon.to_owned()));
            }
            if let Some(urgency) = p.opt("urgency") {
                options.insert("urgency", Value::from(urgency.to_owned()));
            }
            let body = p.args.get(1).map_or("", String::as_str);
            let (id,): (u32,) = crate::call_for(p, "Notify", &(p.args[0].as_str(), body, options), timeout)?;
            Ok(Done { json: serde_json::json!({ "id": id }), text: String::new() })
        }
        _ => Err(Failure::not_supported()),
    }
}

/// The State record's key for toggle `name`.
fn state_key(name: &str) -> &str {
    if name == "mute" { "muted" } else { name }
}

/// Whether toggle `name` is on now (`GetState`); `not-supported` when the
/// state does not say (nothing to flip).
fn current(p: &cli::Parsed, name: &str, timeout: f64) -> Result<bool, Failure> {
    let (state,): (HashMap<String, OwnedValue>,) = crate::call_for(p, "GetState", &(crate::options(p),), timeout)?;
    state.get(state_key(name)).and_then(|v| bool::try_from(v.clone()).ok()).ok_or_else(Failure::not_supported)
}

/// Whether `text` starts with a URI scheme (`https:`, `mailto:`).
fn has_scheme(text: &str) -> bool {
    text.split_once(':').is_some_and(|(s, _)| s.len() > 1 && s.starts_with(|c: char| c.is_ascii_alphabetic()) && s.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)))
}

/// A file, folder or address argument as the shell takes it: URIs as
/// given, paths made absolute (`~` is the home folder).
fn uri_arg(text: &str) -> Result<String, Failure> {
    if has_scheme(text) && !std::path::Path::new(text).exists() {
        return Ok(text.to_owned());
    }
    crate::read::absolute(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_and_paths() {
        assert_eq!(uri_arg("https://example.org/a").unwrap(), "https://example.org/a");
        assert_eq!(uri_arg("mailto:a@example.org").unwrap(), "mailto:a@example.org");
        assert_eq!(uri_arg("/etc/hostname").unwrap(), "/etc/hostname");
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(uri_arg("notes.txt").unwrap(), cwd.join("notes.txt").to_string_lossy());
        assert!(!has_scheme("C") && !has_scheme("a:b") && has_scheme("file:///x"));
        assert_eq!(state_key("mute"), "muted");
        assert_eq!(state_key("wifi"), "wifi");
    }
}
