//! The clipboard commands (helixdesktop/spec `contracts/agent-access/cli.md`;
//! helixdesktop/desktop#151): `clipboard get`, `clipboard types` and
//! `clipboard set`, each one `GetClipboard` or `SetClipboard` call.
//!
//! Text output is the content itself, as it is (no line break added), so
//! `helix-agent --text clipboard get | …` and `… | helix-agent clipboard
//! set` work in pipes; binary content goes to a file or a pipe, never to a
//! terminal.

use std::collections::HashMap;
use std::io::{Read, Write};

use helix_i18n::{tr, trf};
use zbus::zvariant::{OwnedValue, Value};

use crate::errors::Failure;
use crate::{cli, json, Done};

/// The most `set` sends (the shell's limit).
const MAX_BYTES: usize = 16 << 20;

type Record = HashMap<String, OwnedValue>;

fn text(r: &Record, key: &str) -> Option<String> {
    r.get(key).and_then(|v| String::try_from(v.clone()).ok())
}

/// The record's content: its text's bytes, or its data.
fn content(r: &Record) -> Vec<u8> {
    match text(r, "text") {
        Some(t) => t.into_bytes(),
        None => r.get("data").and_then(|v| <Vec<u8>>::try_from(v.clone()).ok()).unwrap_or_default(),
    }
}

/// Run the command at `path`; `json` is the output format.
pub fn run(p: &cli::Parsed, path: &str, json_out: bool) -> Result<Option<Done>, Failure> {
    let timeout = p.timeout.unwrap_or(crate::DEFAULT_TIMEOUT);
    let mut options = crate::options(p);
    match path {
        "clipboard types" => {
            options.insert("types-only", Value::from(true));
            let (record,): (Record,) = crate::call_for(p, "GetClipboard", &(options,), timeout)?;
            let types = record.get("types").and_then(|v| <Vec<String>>::try_from(v.clone()).ok()).unwrap_or_default();
            let text = types.iter().map(|t| format!("{t}\n")).collect();
            Ok(Some(Done { json: serde_json::json!({ "types": types }), text }))
        }
        "clipboard get" => {
            if let Some(mime) = p.opt("type") {
                options.insert("type", Value::from(mime.to_owned()));
            }
            let (record,): (Record,) = crate::call_for(p, "GetClipboard", &(options,), timeout)?;
            if let Some(file) = p.opt("output") {
                let path = std::path::PathBuf::from(crate::read::absolute(file)?);
                std::fs::write(&path, content(&record)).map_err(|e| Failure::new("failed", trf!("Cannot write {path}: {error}", path = path.display(), error = e)))?;
                let mut json = json::record(&record);
                if let Some(o) = json.as_object_mut() {
                    o.remove("text");
                    o.remove("data");
                    o.insert("path".into(), serde_json::Value::from(path.to_string_lossy().into_owned()));
                }
                return Ok(Some(Done { json, text: String::new() }));
            }
            if json_out {
                return Ok(Some(Done { json: json::record(&record), text: String::new() }));
            }
            // Text output: the content as it is.
            let binary = text(&record, "text").is_none();
            // SAFETY: isatty on standard output.
            if binary && unsafe { libc::isatty(1) } == 1 {
                return Err(Failure::usage(tr("The clipboard holds binary data; use --output FILE or redirect standard output")));
            }
            let mut out = std::io::stdout().lock();
            out.write_all(&content(&record)).and_then(|_| out.flush()).map_err(|e| Failure::new("failed", e.to_string()))?;
            Ok(None)
        }
        "clipboard set" => {
            let data = match p.args.first() {
                Some(t) => t.clone().into_bytes(),
                None => {
                    let mut data = Vec::new();
                    std::io::stdin().lock().take(MAX_BYTES as u64 + 1).read_to_end(&mut data).map_err(|e| Failure::new("failed", e.to_string()))?;
                    data
                }
            };
            if data.len() > MAX_BYTES {
                return Err(Failure::usage(trf!("At most {n} MiB can be put on the clipboard", n = MAX_BYTES >> 20)));
            }
            let mime = p.opt("type").unwrap_or("");
            crate::call_for::<_, ()>(p, "SetClipboard", &(mime, data, options), timeout)?;
            Ok(Some(Done { json: serde_json::json!({}), text: String::new() }))
        }
        _ => Err(Failure::not_supported()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contents() {
        let mut r: Record = HashMap::new();
        r.insert("text".into(), Value::from("héllo").try_into().unwrap());
        assert_eq!(content(&r), "héllo".as_bytes());
        let mut r: Record = HashMap::new();
        r.insert("data".into(), Value::from(vec![1u8, 2]).try_into().unwrap());
        assert_eq!(content(&r), [1, 2]);
        assert!(content(&HashMap::new()).is_empty());
    }
}
