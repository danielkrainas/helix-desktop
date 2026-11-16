//! `org.freedesktop.impl.portal.FileChooser` (helixdesktop/spec
//! `contracts/dbus/org.freedesktop.impl.portal.md`, "FileChooser"): the
//! shell's file chooser (`org.helixdesktop.PortalDialog.ChooseFiles`), which
//! browses folders itself in one dialog; this backend passes the app's
//! options and turns the answer into URIs.

use std::collections::HashMap;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gio::prelude::*;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::{fdo, interface, Connection, ObjectServer};

use crate::dialogs::{answered_choices, check_box, value, Choice};
use crate::screenshot::{response, with_request, Results};
use helix_i18n::tr;

pub use helix_settings::files::Filter;

const SHELL: &str = "org.gnome.Shell";
const DIALOG_PATH: &str = "/org/helixdesktop/PortalDialog";
const DIALOG: &str = "org.helixdesktop.PortalDialog";

/// What is being chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Open { multiple: bool, directory: bool },
    Save,
    SaveFiles,
}

/// A path from a portal byte string (ends with a NUL).
pub fn path_of(bytes: &[u8]) -> PathBuf {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    PathBuf::from(std::ffi::OsString::from_vec(bytes[..end].to_vec()))
}

/// A path as the byte string the shell's dialog takes.
fn path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// The user's answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub paths: Vec<PathBuf>,
    /// The folder last shown.
    pub folder: PathBuf,
    pub filter: Option<usize>,
    pub check: bool,
}

/// What the dialog asks.
pub struct Ask {
    pub title: String,
    pub accept: String,
    pub mode: Mode,
    /// The folder to start in; `None`: the shell decides (Recent when
    /// opening files and GTK's `startup-mode` says so, else Home).
    pub folder: Option<PathBuf>,
    pub name: String,
    pub filters: Vec<Filter>,
    pub filter: Option<usize>,
    /// The check box (label, state), from the choices.
    pub check: Option<(String, bool)>,
}

fn opt<T: TryFrom<OwnedValue>>(results: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
    results.get(key).and_then(|v| T::try_from(v.clone()).ok())
}

/// The `ChooseFiles` options for `ask` (helixdesktop/spec
/// `contracts/dbus/org.helixdesktop.PortalDialog.xml`).
pub fn choose_options(ask: &Ask) -> HashMap<String, OwnedValue> {
    let mode = match ask.mode {
        Mode::Open { directory: true, .. } => "folder",
        Mode::Open { .. } => "open",
        Mode::Save => "save",
        Mode::SaveFiles => "save-files",
    };
    let mut options = HashMap::from([
        ("title".to_owned(), value(Value::from(ask.title.clone()))),
        ("accept".to_owned(), value(Value::from(ask.accept.clone()))),
        ("mode".to_owned(), value(Value::from(mode))),
        ("multiple".to_owned(), value(Value::from(matches!(ask.mode, Mode::Open { multiple: true, directory: false })))),
        ("filters".to_owned(), value(Value::from(ask.filters.clone()))),
        ("filter".to_owned(), value(Value::from(ask.filter.map_or(-1, |i| i as i32)))),
    ]);
    if let Some(folder) = &ask.folder {
        options.insert("folder".into(), value(Value::from(path_bytes(folder))));
    }
    if ask.mode == Mode::Save {
        options.insert("name".into(), value(Value::from(ask.name.clone())));
    }
    if let Some((label, state)) = &ask.check {
        options.insert("check-label".into(), value(Value::from(label.clone())));
        options.insert("check".into(), value(Value::from(*state)));
    }
    options
}

/// The answer `ChooseFiles` gave (`None`: canceled).
pub fn outcome(response: u32, results: &HashMap<String, OwnedValue>) -> Option<Outcome> {
    if response != 0 {
        return None;
    }
    let paths: Vec<PathBuf> = opt::<Vec<Vec<u8>>>(results, "paths").unwrap_or_default().iter().map(|b| path_of(b)).filter(|p| !p.as_os_str().is_empty()).collect();
    if paths.is_empty() {
        return None;
    }
    let folder = opt::<Vec<u8>>(results, "folder").map(|b| path_of(&b)).filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| paths[0].parent().map_or_else(home, Path::to_path_buf));
    let filter = opt::<i32>(results, "filter").and_then(|i| usize::try_from(i).ok());
    Some(Outcome { paths, folder, filter, check: opt::<bool>(results, "check").unwrap_or(false) })
}

/// Ask for one image file (PNG, JPEG or SVG), starting in the pictures
/// folder; for other backends (DynamicLauncher's icon).
pub(crate) async fn pick_image(conn: &Connection, title: &str) -> Option<PathBuf> {
    let folder = gio::glib::user_special_dir(gio::glib::UserDirectory::Pictures).filter(|d| d.is_dir()).unwrap_or_else(home);
    let images: Filter = (tr("Images").into(), ["image/png", "image/jpeg", "image/svg+xml"].iter().map(|m| (1, (*m).to_owned())).collect());
    let ask = Ask {
        title: title.into(),
        accept: tr("Choose").into(),
        mode: Mode::Open { multiple: false, directory: false },
        folder: Some(folder),
        name: String::new(),
        filters: vec![images],
        filter: Some(0),
        check: None,
    };
    choose(conn, ask).await.and_then(|o| o.paths.into_iter().next())
}

/// Show the shell's file chooser for `ask`.
async fn choose(conn: &Connection, ask: Ask) -> Option<Outcome> {
    let reply = conn.call_method(Some(SHELL), DIALOG_PATH, Some(DIALOG), "ChooseFiles", &(choose_options(&ask),)).await;
    match reply.and_then(|r| r.body().deserialize::<(u32, HashMap<String, OwnedValue>)>()) {
        Ok((code, results)) => outcome(code, &results),
        Err(err) => {
            tracing::warn!(%err, "the shell cannot show the file chooser");
            None
        }
    }
}

fn opt_str(options: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    options.get(key).and_then(|v| String::try_from(v.clone()).ok()).filter(|s| !s.is_empty())
}

fn opt_bool(options: &HashMap<String, OwnedValue>, key: &str) -> bool {
    options.get(key).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(false)
}

fn opt_path(options: &HashMap<String, OwnedValue>, key: &str) -> Option<PathBuf> {
    options.get(key).and_then(|v| Vec::<u8>::try_from(v.clone()).ok()).map(|b| path_of(&b)).filter(|p| !p.as_os_str().is_empty())
}

fn saving_disabled() -> bool {
    const LOCKDOWN: &str = "org.gnome.desktop.lockdown";
    helix_settings::has_schema(LOCKDOWN) && gio::Settings::new(LOCKDOWN).boolean("disable-save-to-disk")
}

fn uri(path: &Path) -> String {
    gio::File::for_path(path).uri().to_string()
}

pub struct FileChooserBackend {
    pub conn: Connection,
    /// The folder each app last chose in.
    pub folders: Arc<Mutex<HashMap<String, PathBuf>>>,
}

impl FileChooserBackend {
    #[allow(clippy::too_many_arguments)]
    async fn run(&self, server: &ObjectServer, handle: ObjectPath<'_>, app_id: String, title: String, options: HashMap<String, OwnedValue>, mode: Mode, files: Vec<PathBuf>) -> (u32, Results) {
        let filters: Vec<Filter> = options.get("filters").and_then(|v| Vec::<Filter>::try_from(v.clone()).ok()).unwrap_or_default();
        let current: Option<Filter> = options.get("current_filter").and_then(|v| Filter::try_from(v.clone()).ok());
        let mut filters = filters;
        let filter = match current {
            Some(c) => Some(filters.iter().position(|f| *f == c).unwrap_or_else(|| {
                filters.push(c);
                filters.len() - 1
            })),
            None if filters.is_empty() => None,
            None => Some(0),
        };
        let choices: Vec<Choice> = options.get("choices").and_then(|v| Vec::<Choice>::try_from(v.clone()).ok()).unwrap_or_default();
        let check_at = check_box(&choices);
        let check = check_at.map(|i| (choices[i].1.clone(), choices[i].3 == "true"));
        let current_file = opt_path(&options, "current_file");
        let remembered = self.folders.lock().unwrap_or_else(|e| e.into_inner()).get(&app_id).cloned();
        let folder = opt_path(&options, "current_folder").or_else(|| current_file.as_ref().and_then(|f| f.parent().map(Path::to_path_buf))).or(remembered);
        let name = opt_str(&options, "current_name").or_else(|| current_file.as_ref().and_then(|f| f.file_name()).map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
        let accept = opt_str(&options, "accept_label").unwrap_or_else(|| match mode {
            Mode::Open { directory: true, .. } => tr("Select").into(),
            Mode::Open { .. } => tr("Open").into(),
            _ => tr("Save").into(),
        });
        let title = if title.is_empty() { accept.clone() } else { title };
        let ask = Ask { title, accept, mode, folder, name, filters: filters.clone(), filter, check };
        let conn = self.conn.clone();
        let folders = self.folders.clone();
        with_request(server, handle, || async move {
            if !matches!(mode, Mode::Open { .. }) && saving_disabled() {
                return (response::OTHER, Results::new());
            }
            let Some(outcome) = choose(&conn, ask).await else {
                return (response::CANCELLED, Results::new());
            };
            folders.lock().unwrap_or_else(|e| e.into_inner()).insert(app_id, outcome.folder.clone());
            let uris: Vec<String> = match mode {
                Mode::SaveFiles => files.iter().filter_map(|f| f.file_name()).map(|n| uri(&outcome.paths[0].join(n))).collect(),
                _ => outcome.paths.iter().map(|p| uri(p)).collect(),
            };
            tracing::info!(?uris, "files chosen");
            let mut results = Results::from([("uris".to_owned(), value(Value::from(uris)))]);
            if !choices.is_empty() {
                results.insert("choices".into(), value(Value::from(answered_choices(&choices, check_at.map(|i| (i, outcome.check))))));
            }
            if let Some(f) = outcome.filter.and_then(|i| filters.get(i)) {
                results.insert("current_filter".into(), value(Value::from(f.clone())));
            }
            (response::SUCCESS, results)
        })
        .await
    }
}

#[interface(name = "org.freedesktop.impl.portal.FileChooser")]
impl FileChooserBackend {
    async fn open_file(&self, #[zbus(object_server)] server: &ObjectServer, handle: ObjectPath<'_>, app_id: String, _parent_window: String, title: String, options: HashMap<String, OwnedValue>) -> fdo::Result<(u32, Results)> {
        let mode = Mode::Open { multiple: opt_bool(&options, "multiple"), directory: opt_bool(&options, "directory") };
        tracing::info!(app_id, title, ?mode, "open file");
        Ok(self.run(server, handle, app_id, title, options, mode, Vec::new()).await)
    }

    async fn save_file(&self, #[zbus(object_server)] server: &ObjectServer, handle: ObjectPath<'_>, app_id: String, _parent_window: String, title: String, options: HashMap<String, OwnedValue>) -> fdo::Result<(u32, Results)> {
        tracing::info!(app_id, title, "save file");
        Ok(self.run(server, handle, app_id, title, options, Mode::Save, Vec::new()).await)
    }

    async fn save_files(&self, #[zbus(object_server)] server: &ObjectServer, handle: ObjectPath<'_>, app_id: String, _parent_window: String, title: String, options: HashMap<String, OwnedValue>) -> fdo::Result<(u32, Results)> {
        let files: Vec<PathBuf> = options.get("files").and_then(|v| Vec::<Vec<u8>>::try_from(v.clone()).ok()).unwrap_or_default().iter().map(|b| path_of(b)).collect();
        tracing::info!(app_id, title, files = files.len(), "save files");
        Ok(self.run(server, handle, app_id, title, options, Mode::SaveFiles, files).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_and_answers() {
        let pdf: Filter = ("PDF".into(), vec![(0, "*.pdf".into())]);
        let ask = Ask { title: "Save Report".into(), accept: "Save".into(), mode: Mode::Save, folder: Some(PathBuf::from("/home/ada/Documents")), name: "Untitled.odt".into(), filters: vec![pdf.clone()], filter: Some(0), check: Some(("Compress".into(), true)) };
        let o = choose_options(&ask);
        assert_eq!(String::try_from(o["mode"].clone()).unwrap(), "save");
        assert_eq!(Vec::<u8>::try_from(o["folder"].clone()).unwrap(), b"/home/ada/Documents");
        assert_eq!(String::try_from(o["name"].clone()).unwrap(), "Untitled.odt");
        assert_eq!(Vec::<Filter>::try_from(o["filters"].clone()).unwrap(), [pdf]);
        assert_eq!((i32::try_from(o["filter"].clone()).unwrap(), bool::try_from(o["check"].clone()).unwrap()), (0, true));
        let open = choose_options(&Ask { mode: Mode::Open { multiple: true, directory: false }, folder: None, ..ask });
        assert!(!open.contains_key("folder") && !open.contains_key("name"));
        assert_eq!((String::try_from(open["mode"].clone()).unwrap().as_str(), bool::try_from(open["multiple"].clone()).unwrap()), ("open", true));
        let results = HashMap::from([
            ("paths".to_owned(), value(Value::from(vec![b"/home/ada/a b.txt".to_vec(), b"/home/ada/c.txt".to_vec()]))),
            ("folder".to_owned(), value(Value::from(b"/home/ada".to_vec()))),
            ("filter".to_owned(), value(Value::from(0i32))),
            ("check".to_owned(), value(Value::from(true))),
        ]);
        let out = outcome(0, &results).unwrap();
        assert_eq!(out, Outcome { paths: vec!["/home/ada/a b.txt".into(), "/home/ada/c.txt".into()], folder: "/home/ada".into(), filter: Some(0), check: true });
        assert_eq!(outcome(1, &results), None);
        assert_eq!(outcome(0, &HashMap::new()), None, "no paths: canceled");
        assert_eq!(path_of(b"/home/ada/a b.txt\0"), Path::new("/home/ada/a b.txt"));
    }
}
