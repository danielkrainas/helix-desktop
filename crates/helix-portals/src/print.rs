//! `org.freedesktop.impl.portal.Print` (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.impl.portal.md`, "Print"): the user
//! picks a printer (CUPS, through its `lpstat` and `lp` commands) or a
//! file, and the number of copies.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gio::prelude::*;
use zbus::zvariant::{ObjectPath, OwnedFd, OwnedValue, Value};
use zbus::{fdo, interface, Connection, ObjectServer};

use crate::dialogs::{dialog, value};
use crate::screenshot::{response, with_request, Results};

/// The row that saves instead of printing.
pub const TO_FILE: &str = "Print to File";

/// From `lpstat -e` and `lpstat -d` output: the printers, the default one
/// first.
pub fn printers(list: &str, default: &str) -> Vec<String> {
    let default = default.rsplit_once(':').map(|(_, d)| d.trim().to_owned()).filter(|d| !d.is_empty() && !default.contains("no system default"));
    let mut all: Vec<String> = list.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect();
    if let Some(d) = default {
        if let Some(i) = all.iter().position(|p| *p == d) {
            let p = all.remove(i);
            all.insert(0, p);
        }
    }
    all
}

fn command_output(program: &str, args: &[&str]) -> String {
    std::process::Command::new(program).args(args).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
}

fn find_printers() -> Vec<String> {
    printers(&command_output("lpstat", &["-e"]), &command_output("lpstat", &["-d"]))
}

/// A number of copies typed by the user (1 to 999).
pub fn copies(text: &str) -> Option<u32> {
    text.trim().parse().ok().filter(|n| (1..=999).contains(n))
}

/// Where to print: a printer, or a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Printer(String),
    File,
}

/// What the user chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub target: Target,
    pub copies: u32,
}

/// The suffix for a document: `pdf` or `ps` from its first bytes.
pub fn suffix(head: &[u8]) -> &'static str {
    if head.starts_with(b"%!PS") {
        "ps"
    } else {
        "pdf"
    }
}

/// A file name from a document title.
pub fn file_stem(title: &str) -> String {
    let stem: String = title.chars().map(|c| if c == '/' || c.is_control() { '-' } else { c }).collect::<String>().trim().to_owned();
    if stem.is_empty() {
        "output".into()
    } else {
        stem
    }
}

fn documents_dir() -> PathBuf {
    gio::glib::user_special_dir(gio::glib::UserDirectory::Documents).filter(|d| d.is_dir()).or_else(|| std::env::var_os("HOME").map(PathBuf::from)).unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// `dir/stem.ext`, or with " (2)" etc. when taken.
pub fn free_name(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let mut path = dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem} ({n}).{ext}"));
        n += 1;
    }
    path
}

/// Print (or save) the document in `data`; the file written, if any.
fn run(choice: &Choice, title: &str, data: &[u8]) -> Result<Option<PathBuf>, String> {
    match &choice.target {
        Target::File => {
            let path = free_name(&documents_dir(), &file_stem(title), suffix(data));
            std::fs::write(&path, data).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            Ok(Some(path))
        }
        Target::Printer(printer) => {
            use std::io::Write;
            let n = choice.copies.to_string();
            let mut child = std::process::Command::new("lp")
                .args(["-d", printer, "-n", &n, "-t", title, "-"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .spawn()
                .map_err(|e| format!("cannot run lp: {e}"))?;
            child.stdin.take().ok_or("no stdin")?.write_all(data).map_err(|e| e.to_string())?;
            let status = child.wait().map_err(|e| e.to_string())?;
            status.success().then_some(None).ok_or_else(|| format!("lp failed: {status}"))
        }
    }
}

fn printing_disabled() -> bool {
    const LOCKDOWN: &str = "org.gnome.desktop.lockdown";
    helix_settings::has_schema(LOCKDOWN) && gio::Settings::new(LOCKDOWN).boolean("disable-printing")
}

/// Ask the user; `None` when canceled.
async fn ask(conn: &Connection, title: &str, accept: &str, preferred: Option<&str>) -> Option<Choice> {
    let mut rows = find_printers();
    rows.push(TO_FILE.into());
    let selected = preferred.and_then(|p| rows.iter().position(|r| r == p)).unwrap_or(0);
    let mut warning = String::new();
    let mut copies_text = "1".to_owned();
    loop {
        let options = HashMap::from([
            ("title".to_owned(), value(Value::from("Print"))),
            ("message".to_owned(), value(Value::from(if title.is_empty() { "Choose a printer.".to_owned() } else { format!("Print “{title}” on:") }))),
            ("rows".to_owned(), value(Value::from(rows.clone()))),
            ("selected".to_owned(), value(Value::from(selected as i32))),
            ("entry-label".to_owned(), value(Value::from("Copies"))),
            ("entry".to_owned(), value(Value::from(copies_text.clone()))),
            ("warning".to_owned(), value(Value::from(warning.clone()))),
            ("accept".to_owned(), value(Value::from(accept))),
            ("cancel".to_owned(), value(Value::from("Cancel"))),
        ]);
        let results = dialog(conn, options).await?;
        let row = results.get("selected").and_then(|v| i32::try_from(v.clone()).ok()).and_then(|i| usize::try_from(i).ok()).unwrap_or(0);
        copies_text = results.get("entry").and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
        let Some(copies) = copies(&copies_text) else {
            warning = "The number of copies must be 1 to 999.".into();
            continue;
        };
        let target = match rows.get(row) {
            Some(name) if name != TO_FILE => Target::Printer(name.clone()),
            _ => Target::File,
        };
        return Some(Choice { target, copies });
    }
}

#[derive(Default)]
pub struct Prepared {
    next: u32,
    choices: HashMap<u32, Choice>,
}

pub struct PrintBackend {
    pub conn: Connection,
    pub prepared: Arc<Mutex<Prepared>>,
}

fn opt_str(options: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    options.get(key).and_then(|v| String::try_from(v.clone()).ok()).filter(|s| !s.is_empty())
}

#[interface(name = "org.freedesktop.impl.portal.Print")]
impl PrintBackend {
    #[allow(clippy::too_many_arguments)]
    async fn prepare_print(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: ObjectPath<'_>,
        app_id: String,
        _parent_window: String,
        title: String,
        settings: HashMap<String, OwnedValue>,
        page_setup: HashMap<String, OwnedValue>,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        tracing::info!(app_id, title, "prepare print");
        let accept = opt_str(&options, "accept_label").unwrap_or_else(|| "Print".into());
        let preferred = opt_str(&settings, "printer");
        let conn = self.conn.clone();
        let prepared = self.prepared.clone();
        Ok(with_request(server, handle, || async move {
            if printing_disabled() {
                return (response::OTHER, Results::new());
            }
            let Some(choice) = ask(&conn, &title, &accept, preferred.as_deref()).await else {
                return (response::CANCELLED, Results::new());
            };
            let mut settings = settings;
            let printer = match &choice.target {
                Target::Printer(p) => p.clone(),
                Target::File => TO_FILE.to_owned(),
            };
            settings.insert("printer".into(), value(Value::from(printer)));
            settings.insert("n-copies".into(), value(Value::from(choice.copies.to_string())));
            let token = {
                let mut p = prepared.lock().unwrap_or_else(|e| e.into_inner());
                p.next += 1;
                let token = p.next;
                p.choices.insert(token, choice);
                token
            };
            let results = Results::from([
                ("settings".to_owned(), value(Value::from(settings))),
                ("page-setup".to_owned(), value(Value::from(page_setup))),
                ("token".to_owned(), value(Value::from(token))),
            ]);
            (response::SUCCESS, results)
        })
        .await)
    }

    #[allow(clippy::too_many_arguments)]
    async fn print(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: ObjectPath<'_>,
        app_id: String,
        _parent_window: String,
        title: String,
        fd: OwnedFd,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        let token = options.get("token").and_then(|v| u32::try_from(v.clone()).ok());
        let prepared = token.and_then(|t| self.prepared.lock().unwrap_or_else(|e| e.into_inner()).choices.remove(&t));
        tracing::info!(app_id, title, ?token, prepared = prepared.is_some(), "print");
        let conn = self.conn.clone();
        Ok(with_request(server, handle, || async move {
            if printing_disabled() {
                return (response::OTHER, Results::new());
            }
            let choice = match prepared {
                Some(c) => c,
                None => match ask(&conn, &title, "Print", None).await {
                    Some(c) => c,
                    None => return (response::CANCELLED, Results::new()),
                },
            };
            let done = tokio::task::spawn_blocking(move || {
                let mut data = Vec::new();
                let mut file = std::fs::File::from(std::os::fd::OwnedFd::from(fd));
                use std::io::Seek;
                let _ = file.seek(std::io::SeekFrom::Start(0));
                file.read_to_end(&mut data).map_err(|e| e.to_string())?;
                run(&choice, &title, &data).map(|path| (choice, path))
            })
            .await;
            match done {
                Ok(Ok((choice, path))) => {
                    tracing::info!(?choice, ?path, "printed");
                    (response::SUCCESS, Results::new())
                }
                Ok(Err(err)) => {
                    tracing::warn!(%err, "cannot print");
                    (response::OTHER, Results::new())
                }
                Err(_) => (response::OTHER, Results::new()),
            }
        })
        .await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printer_lists() {
        assert_eq!(printers("Office\nLab_Color\n", "system default destination: Lab_Color\n"), ["Lab_Color", "Office"]);
        assert_eq!(printers("Office\n", "no system default destination\n"), ["Office"]);
        assert!(printers("", "").is_empty());
    }

    #[test]
    fn choices_and_files() {
        assert_eq!(copies(" 3 "), Some(3));
        assert_eq!(copies("0"), None);
        assert_eq!(copies("many"), None);
        assert_eq!(suffix(b"%PDF-1.7"), "pdf");
        assert_eq!(suffix(b"%!PS-Adobe-3.0"), "ps");
        assert_eq!(file_stem("Q3/Q4 report"), "Q3-Q4 report");
        assert_eq!(file_stem("  "), "output");
        let dir = std::env::temp_dir().join(format!("helix-print-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(free_name(&dir, "a", "pdf"), dir.join("a.pdf"));
        std::fs::write(dir.join("a.pdf"), b"x").unwrap();
        assert_eq!(free_name(&dir, "a", "pdf"), dir.join("a (2).pdf"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
