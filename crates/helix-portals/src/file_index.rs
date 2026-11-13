//! Searching the desktop's file index (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.impl.portal.md`, FileChooser, "Search"):
//! localsearch (Tracker) answers SPARQL queries on its D-Bus endpoint;
//! results come back through a pipe.

use std::io::Read;
use std::path::{Path, PathBuf};

use gio::prelude::*;
use zbus::Connection;

/// Index services, newest first. Neither is started for a search.
pub const SERVICES: [&str; 2] = ["org.freedesktop.LocalSearch3", "org.freedesktop.Tracker3.Miner.Files"];
const ENDPOINT_PATH: &str = "/org/freedesktop/Tracker3/Endpoint";
const ENDPOINT: &str = "org.freedesktop.Tracker3.Endpoint";

/// The most results asked of the index.
const ASKED: usize = 1000;

/// The full-text pattern for `text`: each word (letters and digits) as a
/// word prefix, all of them required. `None` without words.
pub fn pattern(text: &str) -> Option<String> {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| format!("{}*", w.to_lowercase()))
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

/// A SPARQL string literal of `s`.
fn literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The query for files below folder `folder_uri` whose names or contents
/// match `pattern`.
pub fn query(folder_uri: &str, pattern: &str) -> String {
    let prefix = format!("{}/", folder_uri.trim_end_matches('/'));
    format!(
        "SELECT DISTINCT ?url WHERE {{ \
         {{ ?file nfo:fileName ?name ; fts:match {p} }} UNION {{ ?element fts:match {p} ; nie:isStoredAs ?file }} \
         ?file nie:url ?url . FILTER(STRSTARTS(?url, {prefix})) }} LIMIT {ASKED}",
        p = literal(pattern),
        prefix = literal(&prefix),
    )
}

/// The first column of each row of a query's results as the endpoint
/// writes them: per row, the number of columns, each column's value type,
/// each value's end offset (from the first value's start), then the values,
/// each ending in a NUL; numbers are 32-bit, in the machine's byte order.
pub fn first_column(data: &[u8]) -> Vec<String> {
    let int = |at: usize| -> Option<usize> { Some(i32::from_ne_bytes(data.get(at..at + 4)?.try_into().ok()?).max(0) as usize) };
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let Some(columns) = int(at).filter(|n| *n > 0 && *n < 1024) else { break };
        let offsets = at + 4 + 4 * columns;
        let values = offsets + 4 * columns;
        let (Some(first_end), Some(last_end)) = (int(offsets), int(offsets + 4 * (columns - 1))) else { break };
        let Some(first) = data.get(values..values + first_end) else { break };
        out.push(String::from_utf8_lossy(first).into_owned());
        at = values + last_end + 1;
    }
    out
}

/// Whether `name` has an owner (without starting it).
async fn running(conn: &Connection, name: &str) -> bool {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await else { return false };
    let Ok(name) = zbus::names::BusName::try_from(name) else { return false };
    dbus.name_has_owner(name).await.unwrap_or(false)
}

/// Run `sparql` on the running index: the first column of the results.
/// `None` when no index runs or it fails.
async fn run(conn: &Connection, sparql: &str) -> Option<Vec<String>> {
    let mut service = None;
    for name in SERVICES {
        if running(conn, name).await {
            service = Some(name);
            break;
        }
    }
    let service = service?;
    let (mut reader, writer) = std::io::pipe().ok()?;
    // The endpoint writes while (or after) it answers: read on a thread.
    let read = std::thread::spawn(move || {
        let mut data = Vec::new();
        reader.read_to_end(&mut data).map(|_| data)
    });
    let writer = std::os::fd::OwnedFd::from(writer);
    let options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> = Default::default();
    let reply = conn
        .call_method(Some(service), ENDPOINT_PATH, Some(ENDPOINT), "Query", &(sparql, zbus::zvariant::Fd::from(&writer), options))
        .await;
    drop(writer);
    if let Err(err) = reply {
        tracing::debug!(service, %err, "file index query failed");
        // The thread ends once the endpoint lets go of the pipe.
        return None;
    }
    let data = tokio::task::spawn_blocking(move || read.join()).await.ok()?.ok()?.ok()?;
    Some(first_column(&data))
}

/// Files below `dir` whose names or contents have words starting with the
/// words of `text`, from the index; `None` when there is no index to ask.
pub async fn search(conn: &Connection, dir: &Path, text: &str) -> Option<Vec<PathBuf>> {
    let pattern = pattern(text)?;
    let folder = gio::File::for_path(dir).uri().to_string();
    let urls = run(conn, &query(&folder, &pattern)).await?;
    Some(urls.iter().filter_map(|u| gio::File::for_uri(u).path()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        assert_eq!(pattern("Budget"), Some("budget*".into()));
        assert_eq!(pattern("  q3 report_final.pdf "), Some("q3* report* final* pdf*".into()));
        assert_eq!(pattern("\"*) OR ("), Some("or*".into()));
        assert_eq!(pattern(" -- "), None);
        let q = query("file:///home/u/My%20Docs/", "budget*");
        assert!(q.contains("STRSTARTS(?url, \"file:///home/u/My%20Docs/\")"), "{q}");
        assert!(q.contains("fts:match \"budget*\""));
        assert_eq!(literal("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn results() {
        // Two rows of (url, integer), as an endpoint wrote them.
        let mut data = Vec::new();
        for url in ["file:///tmp/a/b/budget-notes.md", "file:///tmp/a/Report.txt"] {
            for n in [2i32, 2, 3, url.len() as i32, url.len() as i32 + 3] {
                data.extend(n.to_ne_bytes());
            }
            data.extend(url.as_bytes());
            data.extend(b"\x0042\x00");
        }
        assert_eq!(first_column(&data), ["file:///tmp/a/b/budget-notes.md", "file:///tmp/a/Report.txt"]);
        assert!(first_column(&[]).is_empty());
        // Cut short: what came whole is kept.
        assert_eq!(first_column(&data[..data.len() - 10]), ["file:///tmp/a/b/budget-notes.md"]);
    }
}
