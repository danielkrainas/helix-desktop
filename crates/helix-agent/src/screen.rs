//! The *Screen and input* commands (helixdesktop/spec
//! `contracts/agent-access/cli.md`; helixdesktop/desktop#150):
//! `screenshot` (`Screenshot`) and `input key`, `type`, `click`, `move`,
//! `scroll`, `drag` and `sequence` (each one `Input` call with its
//! events). The shell decides what may be seen and where input may go;
//! the CLI only builds the call.

use std::collections::HashMap;
use std::io::Write;

use helix_i18n::{tr, trf};
use zbus::zvariant::{OwnedValue, Value};

use crate::errors::Failure;
use crate::read::window_id;
use crate::{cli, json, Done};

type Record = HashMap<String, OwnedValue>;
type Event = HashMap<&'static str, Value<'static>>;

use crate::cli::BUTTONS;
/// The steps of `input sequence`.
pub const STEPS: &[&str] = &["move", "click", "double-click", "mouse-down", "mouse-up", "key", "key-down", "key-up", "scroll", "type", "wait"];

fn number(word: &str) -> Result<f64, Failure> {
    word.parse::<f64>().ok().filter(|n| n.is_finite()).ok_or_else(|| Failure::usage(trf!("“{value}” is not a number", value = word)))
}

fn steps_number(word: &str) -> Result<i32, Failure> {
    word.parse::<i32>().map_err(|_| Failure::usage(trf!("“{value}” is not a whole number", value = word)))
}

fn event(kind: &str) -> Event {
    HashMap::from([("type", Value::from(kind.to_owned()))])
}

fn at(e: &mut Event, x: f64, y: f64) {
    e.insert("x", Value::from(x));
    e.insert("y", Value::from(y));
}

/// `X,Y,WIDTH,HEIGHT`.
fn area(text: &str) -> Result<(i32, i32, i32, i32), Failure> {
    let bad = || Failure::usage(trf!("“{value}” is not an area: X,Y,WIDTH,HEIGHT", value = text));
    let parts: Vec<i32> = text.split(',').map(|p| p.trim().parse::<i32>()).collect::<Result<_, _>>().map_err(|_| bad())?;
    match parts[..] {
        [x, y, w, h] if w > 0 && h > 0 => Ok((x, y, w, h)),
        _ => Err(bad()),
    }
}

/// `X,Y`.
fn point(text: &str) -> Result<(f64, f64), Failure> {
    let bad = || Failure::usage(trf!("“{value}” is not a point: X,Y", value = text));
    let (x, y) = text.split_once(',').ok_or_else(bad)?;
    Ok((number(x.trim()).map_err(|_| bad())?, number(y.trim()).map_err(|_| bad())?))
}

fn button(name: &str) -> Result<String, Failure> {
    if BUTTONS.contains(&name) {
        Ok(name.to_owned())
    } else {
        Err(Failure::usage(trf!("“{value}” is not one of: {choices}", value = name, choices = BUTTONS.join(", "))))
    }
}

/// `input sequence`'s words as events.
pub fn sequence(words: &[String]) -> Result<Vec<Event>, Failure> {
    let mut out = Vec::new();
    let mut i = 0;
    let need = |i: usize, what: &str| -> Result<&String, Failure> { words.get(i).ok_or_else(|| Failure::usage(trf!("“{step}” needs more", step = what))) };
    // An optional button name, then an optional X Y.
    let button_at = |i: &mut usize, e: &mut Event| -> Result<(), Failure> {
        if let Some(word) = words.get(*i).filter(|w| BUTTONS.contains(&w.as_str())) {
            e.insert("button", Value::from(word.clone()));
            *i += 1;
        }
        if let (Some(x), Some(y)) = (words.get(*i), words.get(*i + 1)) {
            if let (Ok(x), Ok(y)) = (x.parse::<f64>(), y.parse::<f64>()) {
                at(e, x, y);
                *i += 2;
            }
        }
        Ok(())
    };
    while i < words.len() {
        let step = words[i].as_str();
        i += 1;
        match step {
            "move" => {
                let mut e = event("move");
                at(&mut e, number(need(i, step)?)?, number(need(i + 1, step)?)?);
                i += 2;
                out.push(e);
            }
            "click" | "double-click" => {
                let mut e = event("click");
                button_at(&mut i, &mut e)?;
                if step == "double-click" {
                    e.insert("count", Value::from(2u32));
                }
                out.push(e);
            }
            "mouse-down" | "mouse-up" => {
                let mut e = event(if step == "mouse-down" { "down" } else { "up" });
                button_at(&mut i, &mut e)?;
                e.entry("button").or_insert_with(|| Value::from("left"));
                out.push(e);
            }
            "key" => {
                let mut e = event("key");
                e.insert("keys", Value::from(need(i, step)?.clone()));
                i += 1;
                out.push(e);
            }
            "key-down" | "key-up" => {
                let mut e = event(if step == "key-down" { "down" } else { "up" });
                e.insert("key", Value::from(need(i, step)?.clone()));
                i += 1;
                out.push(e);
            }
            "scroll" => {
                let mut e = event("scroll");
                e.insert("dx", Value::from(steps_number(need(i, step)?)?));
                e.insert("dy", Value::from(steps_number(need(i + 1, step)?)?));
                i += 2;
                out.push(e);
            }
            "type" => {
                let mut e = event("text");
                e.insert("text", Value::from(need(i, step)?.clone()));
                i += 1;
                out.push(e);
            }
            "wait" => {
                let ms = need(i, step)?.parse::<u32>().map_err(|_| Failure::usage(trf!("“{value}” is not a number of milliseconds", value = words[i])))?;
                let mut e = event("wait");
                e.insert("ms", Value::from(ms));
                i += 1;
                out.push(e);
            }
            other => return Err(Failure::usage(trf!("unknown step “{step}”; steps: {steps}", step = other, steps = STEPS.join(", ")))),
        }
    }
    if out.is_empty() {
        return Err(Failure::usage(tr("no steps")));
    }
    Ok(out)
}

/// The `input` command at `path`'s events.
pub fn events(p: &cli::Parsed, path: &str) -> Result<Vec<Event>, Failure> {
    let args = &p.args;
    let mut out = Vec::new();
    match path {
        "input key" => {
            for keys in args {
                let mut e = event("key");
                e.insert("keys", Value::from(keys.clone()));
                out.push(e);
            }
        }
        "input type" => {
            let mut e = event("text");
            e.insert("text", Value::from(args[0].clone()));
            out.push(e);
        }
        "input click" => {
            let mut e = event("click");
            at(&mut e, number(&args[0])?, number(&args[1])?);
            if let Some(b) = p.opt("button") {
                e.insert("button", Value::from(button(b)?));
            }
            if let Some(n) = p.opt("count") {
                let count = n.parse::<u32>().ok().filter(|c| (1..=3).contains(c)).ok_or_else(|| Failure::usage(trf!("“{value}” is not a number from 1 to 3", value = n)))?;
                e.insert("count", Value::from(count));
            }
            out.push(e);
        }
        "input move" => {
            let mut e = event("move");
            at(&mut e, number(&args[0])?, number(&args[1])?);
            out.push(e);
        }
        "input scroll" => {
            let mut e = event("scroll");
            e.insert("dx", Value::from(steps_number(&args[0])?));
            e.insert("dy", Value::from(steps_number(&args[1])?));
            if let Some(a) = p.opt("at") {
                let (x, y) = point(a)?;
                at(&mut e, x, y);
            }
            out.push(e);
        }
        "input drag" => {
            let b = button(p.opt("button").unwrap_or("left"))?;
            let mut down = event("down");
            down.insert("button", Value::from(b.clone()));
            at(&mut down, number(&args[0])?, number(&args[1])?);
            let mut to = event("move");
            at(&mut to, number(&args[2])?, number(&args[3])?);
            let mut up = event("up");
            up.insert("button", Value::from(b));
            out.extend([down, to, up]);
        }
        "input sequence" => out = sequence(args)?,
        _ => return Err(Failure::not_supported()),
    }
    Ok(out)
}

/// Run the command at `path`; `json` is the output format.
pub fn run(p: &cli::Parsed, path: &str, json_out: bool) -> Result<Option<Done>, Failure> {
    let timeout = p.timeout.unwrap_or(crate::DEFAULT_TIMEOUT);
    let mut options = crate::options(p);
    if let Some(w) = p.opt("window") {
        options.insert("window", Value::from(window_id(w)?));
    }
    if path != "screenshot" {
        let events = events(p, path)?;
        crate::call_for::<_, ()>(p, "Input", &(events, options), timeout)?;
        return Ok(Some(Done { json: serde_json::json!({}), text: String::new() }));
    }
    if let Some(m) = p.opt("monitor") {
        if p.has("window") {
            return Err(Failure::usage(tr("--window and --monitor cannot both be given")));
        }
        options.insert("monitor", Value::from(m.to_owned()));
    }
    if let Some(a) = p.opt("area") {
        let (x, y, w, h) = area(a)?;
        for (k, v) in [("x", x), ("y", y), ("width", w), ("height", h)] {
            options.insert(k, Value::from(v));
        }
    }
    let (record,): (Record,) = crate::call_for(p, "Screenshot", &(options,), timeout)?;
    let png = record.get("png").and_then(|v| <Vec<u8>>::try_from(v.clone()).ok()).unwrap_or_default();
    let int = |k: &str| record.get(k).and_then(|v| i32::try_from(v.clone()).ok()).unwrap_or(0);
    let filled = record.get("filled").and_then(|v| u32::try_from(v.clone()).ok()).unwrap_or(0);
    let mut json = json::record(&record);
    if let Some(file) = p.opt("output") {
        let path = std::path::PathBuf::from(crate::read::absolute(file)?);
        std::fs::write(&path, &png).map_err(|e| Failure::new("failed", trf!("Cannot write {path}: {error}", path = path.display(), error = e)))?;
        if let Some(o) = json.as_object_mut() {
            o.remove("png");
            o.insert("path".into(), serde_json::Value::from(path.to_string_lossy().into_owned()));
        }
        let mut text = trf!("Saved a {width}×{height} screenshot to {path}", width = int("width"), height = int("height"), path = path.display());
        if filled > 0 {
            text.push('\n');
            text.push_str(&trf!("Blacked out: {count}", count = filled));
        }
        return Ok(Some(Done { json, text }));
    }
    if json_out {
        return Ok(Some(Done { json, text: String::new() }));
    }
    // Text output: the PNG itself, to a pipe or a file only.
    // SAFETY: isatty on standard output.
    if unsafe { libc::isatty(1) } == 1 {
        return Err(Failure::usage(tr("A screenshot is binary data; use --output FILE or redirect standard output")));
    }
    let mut out = std::io::stdout().lock();
    out.write_all(&png).and_then(|_| out.flush()).map_err(|e| Failure::new("failed", e.to_string()))?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    fn kind(e: &Event) -> String {
        String::try_from(e["type"].clone()).unwrap()
    }

    #[test]
    fn sequences() {
        let out = sequence(&words("key-down shift click 10 20 key-up shift type hi wait 50 double-click right scroll 0 -3 mouse-down 1 2 move 5 5 mouse-up key ctrl+s")).unwrap();
        let kinds: Vec<String> = out.iter().map(kind).collect();
        assert_eq!(kinds, ["down", "click", "up", "text", "wait", "click", "scroll", "down", "move", "up", "key"]);
        assert_eq!(f64::try_from(out[1]["x"].clone()).unwrap(), 10.0);
        assert_eq!(String::try_from(out[0]["key"].clone()).unwrap(), "shift");
        assert_eq!(u32::try_from(out[5]["count"].clone()).unwrap(), 2);
        assert_eq!(String::try_from(out[5]["button"].clone()).unwrap(), "right");
        assert_eq!(String::try_from(out[7]["button"].clone()).unwrap(), "left");
        assert!(sequence(&words("fly 1 2")).is_err());
        assert!(sequence(&words("move 1")).is_err());
        assert!(sequence(&words("wait soon")).is_err());
        assert!(sequence(&[]).is_err());
    }

    #[test]
    fn areas_and_points() {
        assert_eq!(area("1,2,30,40").unwrap(), (1, 2, 30, 40));
        assert!(area("1,2,0,40").is_err());
        assert!(area("1,2,3").is_err());
        assert_eq!(point("1.5, 2").unwrap(), (1.5, 2.0));
        assert!(point("1").is_err());
    }
}
