//! `org.freedesktop.impl.portal.FileChooser` (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.impl.portal.md`, "FileChooser"): a
//! folder at a time in the shell's dialog.

use std::collections::HashMap;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gio::prelude::*;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::{fdo, interface, Connection, ObjectServer};

use crate::dialogs::{answered_choices, check_box, dialog, value, Choice};
use crate::screenshot::{response, with_request, Results};

/// A filter: its name and patterns, (0, glob) or (1, MIME type).
pub type Filter = (String, Vec<(u32, String)>);

/// `*` and `?` glob matching, ignoring case.
pub fn glob(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some(('*', rest)) => (0..=n.len()).any(|i| go(rest, &n[i..])),
            Some(('?', rest)) => !n.is_empty() && go(rest, &n[1..]),
            Some((c, rest)) => n.first().is_some_and(|m| m == c) && go(rest, &n[1..]),
        }
    }
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    go(&p, &n)
}

/// Whether file `name` passes `filter`.
pub fn passes(filter: &Filter, name: &str) -> bool {
    filter.1.iter().any(|(kind, pattern)| match kind {
        0 => glob(pattern, name),
        _ => {
            let (guess, _) = gio::content_type_guess(Some(name), None);
            gio::content_type_is_mime_type(&guess, pattern)
        }
    })
}

/// A row of the chooser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Parent,
    /// Change the file type shown.
    FilterRow,
    SelectHere,
    SaveHere,
    Folder(String),
    File(String),
    /// A recently used file (the Recent view).
    Recent(PathBuf),
}

/// A place in the dialog's column of places.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    Recent,
    Folder(String, PathBuf),
}

impl Place {
    pub fn label(&self) -> &str {
        match self {
            Place::Recent => "Recent",
            Place::Folder(label, _) => label,
        }
    }
}

/// GTK bookmarks (`~/.config/gtk-3.0/bookmarks`: `file:` URI, then an
/// optional label) that are local folders.
pub fn bookmarks(text: &str) -> Vec<(String, PathBuf)> {
    text.lines()
        .filter_map(|line| {
            let (uri, label) = line.trim().split_once(' ').map_or((line.trim(), None), |(u, l)| (u, Some(l.trim())));
            let path = gio::File::for_uri(uri).path().filter(|_| uri.starts_with("file://"))?;
            let label = label.filter(|l| !l.is_empty()).map(str::to_owned).or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))?;
            Some((label, path))
        })
        .collect()
}

/// The places: Recent (when files are being opened), Home, the user's
/// standard folders, bookmarks, drives and the whole computer; folders
/// that do not exist are left out, and none twice.
pub fn places(home: &Path, standard: &[PathBuf], bookmarks: &[(String, PathBuf)], drives: &[(String, PathBuf)], recent: bool) -> Vec<Place> {
    let mut out = Vec::new();
    if recent {
        out.push(Place::Recent);
    }
    out.push(Place::Folder("Home".into(), home.to_path_buf()));
    let named = |p: &PathBuf| (p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned()), p.clone());
    let candidates = standard.iter().map(named).chain(bookmarks.iter().cloned()).chain(drives.iter().cloned()).chain([("Computer".to_owned(), PathBuf::from("/"))]);
    for (label, path) in candidates {
        let seen = out.iter().any(|p| matches!(p, Place::Folder(_, q) if *q == path));
        if !seen && path.is_dir() {
            out.push(Place::Folder(label, path));
        }
    }
    out
}

/// The standard folders (Desktop, Documents, Downloads, Music, Pictures,
/// Videos) that are not the home folder itself.
fn standard_folders(home: &Path) -> Vec<PathBuf> {
    use gio::glib::UserDirectory as D;
    [D::Desktop, D::Documents, D::Downloads, D::Music, D::Pictures, D::Videos]
        .into_iter()
        .filter_map(gio::glib::user_special_dir)
        .filter(|p| p != home)
        .collect()
}

/// Drives mounted where udisks mounts them for the user.
fn drives() -> Vec<(String, PathBuf)> {
    let user = std::env::var("USER").unwrap_or_default();
    let mut out: Vec<(String, PathBuf)> = [format!("/media/{user}"), format!("/run/media/{user}")]
        .iter()
        .filter(|_| !user.is_empty())
        .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten())
        .filter(|e| e.path().is_dir())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    out.sort();
    out
}

fn user_places(home: &Path, recent: bool) -> Vec<Place> {
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    let marks = std::fs::read_to_string(config.join("gtk-3.0/bookmarks")).map(|t| bookmarks(&t)).unwrap_or_default();
    places(home, &standard_folders(home), &marks, &drives(), recent)
}

/// The most recently used files shown.
pub const RECENT_SHOWN: usize = 100;

/// The `<bookmark>` entries of an XBEL file: (URI, when last modified or
/// added, Unix seconds).
pub fn xbel_entries(text: &str) -> Vec<(String, i64)> {
    let attr = |tag: &str, name: &str| -> Option<String> {
        let at = tag.find(&format!(" {name}=\""))? + name.len() + 3;
        let end = tag[at..].find('"')? + at;
        Some(tag[at..end].replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'"))
    };
    let when = |stamp: Option<String>| stamp.and_then(|s| gio::glib::DateTime::from_iso8601(&s, None).ok()).map_or(0, |d| d.to_unix());
    text.split("<bookmark")
        .skip(1)
        .filter_map(|chunk| {
            let tag = &chunk[..chunk.find('>')?];
            let href = attr(tag, "href")?;
            let time = when(attr(tag, "modified")).max(when(attr(tag, "added")));
            Some((href, time))
        })
        .collect()
}

/// Recently used local files that still exist and pass `filter`, newest
/// first (`recently-used.xbel` under the data folder).
fn recent_files(home: &Path, filter: Option<&Filter>) -> Vec<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share"));
    let Ok(text) = std::fs::read_to_string(data.join("recently-used.xbel")) else { return Vec::new() };
    let mut found: Vec<(i64, PathBuf)> = xbel_entries(&text)
        .into_iter()
        .filter_map(|(uri, when)| {
            let path = gio::File::for_uri(&uri).path().filter(|_| uri.starts_with("file://"))?;
            let name = path.file_name()?.to_string_lossy().into_owned();
            (path.is_file() && filter.is_none_or(|f| passes(f, &name))).then_some((when, path))
        })
        .collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, p)| p).take(RECENT_SHOWN).collect()
}

/// What is being chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Open { multiple: bool, directory: bool },
    Save,
    SaveFiles,
}

/// The rows for folder `dir` with these folder entries (name, is folder).
pub fn rows(dir: &Path, entries: &[(String, bool)], mode: Mode, filter: Option<&Filter>, filter_row: bool) -> Vec<Row> {
    let mut out = Vec::new();
    match mode {
        Mode::Save | Mode::SaveFiles => out.push(Row::SaveHere),
        Mode::Open { directory: true, .. } => out.push(Row::SelectHere),
        Mode::Open { .. } => {}
    }
    if dir.parent().is_some() {
        out.push(Row::Parent);
    }
    if filter_row && mode != Mode::SaveFiles && !matches!(mode, Mode::Open { directory: true, .. }) {
        out.push(Row::FilterRow);
    }
    let mut folders: Vec<&String> = entries.iter().filter(|(n, d)| *d && !n.starts_with('.')).map(|(n, _)| n).collect();
    folders.sort_by_key(|n| n.to_lowercase());
    out.extend(folders.into_iter().map(|n| Row::Folder(n.clone())));
    let files_shown = !matches!(mode, Mode::SaveFiles | Mode::Open { directory: true, .. });
    if files_shown {
        let mut files: Vec<&String> = entries.iter().filter(|(n, d)| !*d && !n.starts_with('.') && filter.is_none_or(|f| passes(f, n))).map(|(n, _)| n).collect();
        files.sort_by_key(|n| n.to_lowercase());
        out.extend(files.into_iter().map(|n| Row::File(n.clone())));
    }
    out
}

pub fn label(row: &Row, filter_name: &str) -> String {
    match row {
        Row::Parent => "Parent Folder".into(),
        Row::FilterRow => format!("File type: {filter_name} (choose to change)"),
        Row::SelectHere => "Select This Folder".into(),
        Row::SaveHere => "Save Here".into(),
        Row::Folder(n) => format!("{n}/"),
        Row::File(n) => n.clone(),
        Row::Recent(p) => {
            let name = p.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            let folder = p.parent().and_then(Path::file_name).map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            format!("{name} — {folder}")
        }
    }
}

/// A typed location, relative to `dir` unless absolute or starting `~`.
pub fn resolve(dir: &Path, typed: &str, home: &Path) -> PathBuf {
    let typed = typed.trim();
    if typed == "~" {
        home.to_path_buf()
    } else if let Some(rest) = typed.strip_prefix("~/") {
        home.join(rest)
    } else {
        dir.join(typed)
    }
}

/// A folder for the message: `~` for the home folder.
pub fn shown_path(dir: &Path, home: &Path) -> String {
    match dir.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => dir.display().to_string(),
    }
}

/// A path from a portal byte string (ends with a NUL).
pub fn path_of(bytes: &[u8]) -> PathBuf {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    PathBuf::from(std::ffi::OsString::from_vec(bytes[..end].to_vec()))
}

fn entries(dir: &Path) -> Vec<(String, bool)> {
    std::fs::read_dir(dir)
        .map(|it| it.flatten().filter_map(|e| Some((e.file_name().into_string().ok()?, e.path().is_dir()))).collect())
        .unwrap_or_default()
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
    pub folder: PathBuf,
    pub name: String,
    pub filters: Vec<Filter>,
    pub filter: Option<usize>,
    /// The check box (label, state), from the choices.
    pub check: Option<(String, bool)>,
}

fn opt<T: TryFrom<OwnedValue>>(results: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
    results.get(key).and_then(|v| T::try_from(v.clone()).ok())
}

async fn choose(conn: &Connection, mut ask: Ask) -> Option<Outcome> {
    let home = home();
    let mut warning = String::new();
    // The row to start on (after going up: the folder left).
    let mut prefer: Option<Row> = None;
    // Recent files are offered when choosing files to open.
    let offer_recent = matches!(ask.mode, Mode::Open { directory: false, .. });
    let mut recent = false;
    loop {
        if !ask.folder.is_dir() {
            ask.folder = home.clone();
        }
        let filter = ask.filter.and_then(|i| ask.filters.get(i));
        let places = user_places(&home, offer_recent);
        let list = if recent {
            let mut list = Vec::new();
            if ask.filters.len() > 1 {
                list.push(Row::FilterRow);
            }
            list.extend(recent_files(&home, filter).into_iter().map(Row::Recent));
            list
        } else {
            rows(&ask.folder, &entries(&ask.folder), ask.mode, filter, ask.filters.len() > 1)
        };
        let place_current = places.iter().position(|p| match p {
            Place::Recent => recent,
            Place::Folder(_, path) => !recent && *path == ask.folder,
        });
        let filter_name = filter.map_or("All Files", |f| f.0.as_str());
        let labels: Vec<String> = list.iter().map(|r| label(r, filter_name)).collect();
        let selected = prefer
            .take()
            .and_then(|p| list.iter().position(|r| *r == p))
            .or_else(|| match ask.mode {
                Mode::Open { .. } => list.iter().position(|r| matches!(r, Row::File(_) | Row::Folder(_) | Row::SelectHere | Row::Recent(_))),
                _ => Some(0),
            })
            .unwrap_or(0);
        let (entry_label, entry) = match ask.mode {
            Mode::Save => ("Name", ask.name.clone()),
            _ => ("Location", String::new()),
        };
        let multiple = matches!(ask.mode, Mode::Open { multiple: true, directory: false });
        let mut options = HashMap::from([
            ("title".to_owned(), value(Value::from(ask.title.clone()))),
            ("message".to_owned(), value(Value::from(if recent { "Recent".to_owned() } else { shown_path(&ask.folder, &home) }))),
            ("places".to_owned(), value(Value::from(places.iter().map(|p| p.label().to_owned()).collect::<Vec<String>>()))),
            ("place-current".to_owned(), value(Value::from(place_current.map_or(-1, |i| i as i32)))),
            ("rows".to_owned(), value(Value::from(labels))),
            ("selected".to_owned(), value(Value::from(selected as i32))),
            ("multiple".to_owned(), value(Value::from(multiple))),
            ("unpickable".to_owned(), value(Value::from(list.iter().enumerate().filter(|(_, r)| !matches!(r, Row::File(_) | Row::Recent(_))).map(|(i, _)| i as i32).collect::<Vec<i32>>()))),
            ("activate-rows".to_owned(), value(Value::from(true))),
            ("entry-label".to_owned(), value(Value::from(entry_label))),
            ("entry".to_owned(), value(Value::from(entry))),
            ("warning".to_owned(), value(Value::from(std::mem::take(&mut warning)))),
            ("accept".to_owned(), value(Value::from(ask.accept.clone()))),
            ("cancel".to_owned(), value(Value::from("Cancel"))),
        ]);
        if let Some((label, state)) = &ask.check {
            options.insert("check-label".into(), value(Value::from(label.clone())));
            options.insert("check".into(), value(Value::from(*state)));
        }
        let results = dialog(conn, options).await?;
        if let (Some(check), Some((_, state))) = (opt::<bool>(&results, "check"), ask.check.as_mut()) {
            *state = check;
        }
        // A place was clicked: show it.
        if let Some(place) = opt::<i32>(&results, "place").and_then(|i| usize::try_from(i).ok()).and_then(|i| places.get(i)) {
            match place {
                Place::Recent => recent = true,
                Place::Folder(_, path) => {
                    recent = false;
                    ask.folder = path.clone();
                }
            }
            if ask.mode == Mode::Save {
                ask.name = opt::<String>(&results, "entry").unwrap_or_default().trim().to_owned();
            }
            continue;
        }
        let row = opt::<i32>(&results, "selected").and_then(|i| usize::try_from(i).ok()).and_then(|i| list.get(i).cloned());
        let typed = opt::<String>(&results, "entry").unwrap_or_default();
        let picked: Vec<Row> = opt::<Vec<i32>>(&results, "picked").unwrap_or_default().into_iter().filter_map(|i| list.get(usize::try_from(i).ok()?).cloned()).collect();
        let done = |paths: Vec<PathBuf>, ask: &Ask| Some(Outcome { paths, folder: ask.folder.clone(), filter: ask.filter, check: ask.check.as_ref().is_some_and(|c| c.1) });
        if ask.mode == Mode::Save {
            ask.name = typed.trim().to_owned();
        } else if !typed.trim().is_empty() {
            // A typed location: go there, or choose it.
            let path = resolve(&ask.folder, &typed, &home);
            if path.is_dir() {
                recent = false;
                ask.folder = path;
            } else if path.is_file() && matches!(ask.mode, Mode::Open { directory: false, .. }) {
                return done(vec![path], &ask);
            } else {
                warning = format!("“{}” was not found.", typed.trim());
            }
            continue;
        }
        let files: Vec<PathBuf> = picked
            .iter()
            .filter_map(|r| match r {
                Row::File(n) => Some(ask.folder.join(n)),
                Row::Recent(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        if !files.is_empty() {
            return done(files, &ask);
        }
        match row {
            Some(Row::Parent) => {
                let from = ask.folder.file_name().map(|n| Row::Folder(n.to_string_lossy().into_owned()));
                if let Some(parent) = ask.folder.parent() {
                    ask.folder = parent.to_path_buf();
                }
                prefer = from;
            }
            Some(Row::FilterRow) => {
                ask.filter = Some(ask.filter.map_or(0, |i| (i + 1) % ask.filters.len().max(1)));
                prefer = Some(Row::FilterRow);
            }
            Some(Row::Folder(n)) => ask.folder = ask.folder.join(n),
            Some(Row::Recent(p)) => return done(vec![p], &ask),
            Some(Row::SelectHere) => return done(vec![ask.folder.clone()], &ask),
            Some(Row::File(n)) if ask.mode == Mode::Save => {
                ask.name = n;
                prefer = Some(Row::SaveHere);
            }
            Some(Row::File(n)) => return done(vec![ask.folder.join(n)], &ask),
            Some(Row::SaveHere) if ask.mode == Mode::SaveFiles => return done(vec![ask.folder.clone()], &ask),
            Some(Row::SaveHere) => {
                if ask.name.is_empty() {
                    warning = "Type a name for the file.".into();
                    continue;
                }
                let target = resolve(&ask.folder, &ask.name, &home);
                if target.is_dir() {
                    ask.folder = target;
                    ask.name.clear();
                    continue;
                }
                if !target.parent().is_some_and(Path::is_dir) {
                    warning = "That folder does not exist.".into();
                    continue;
                }
                if target.exists() && !replace(conn, &target).await {
                    continue;
                }
                return done(vec![target], &ask);
            }
            None => return None,
        }
    }
}

/// Ask before replacing `path`.
async fn replace(conn: &Connection, path: &Path) -> bool {
    let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let folder = path.parent().and_then(Path::file_name).map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let options = HashMap::from([
        ("title".to_owned(), value(Value::from("Replace File?"))),
        ("message".to_owned(), value(Value::from(format!("A file named “{name}” already exists in “{folder}”. Replacing it overwrites its contents.")))),
        ("accept".to_owned(), value(Value::from("Replace"))),
        ("cancel".to_owned(), value(Value::from("Cancel"))),
    ]);
    dialog(conn, options).await.is_some()
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
        let folder = opt_path(&options, "current_folder")
            .or_else(|| current_file.as_ref().and_then(|f| f.parent().map(Path::to_path_buf)))
            .or(remembered)
            .unwrap_or_else(home);
        let name = opt_str(&options, "current_name").or_else(|| current_file.as_ref().and_then(|f| f.file_name()).map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
        let accept = opt_str(&options, "accept_label").unwrap_or_else(|| match mode {
            Mode::Open { directory: true, .. } => "Select".into(),
            Mode::Open { .. } => "Open".into(),
            _ => "Save".into(),
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
    fn places_list() {
        let tmp = std::env::temp_dir().join(format!("helix-places-{}", std::process::id()));
        let (docs, mark, drive) = (tmp.join("Documents"), tmp.join("Projects"), tmp.join("usb"));
        for d in [&docs, &mark, &drive] {
            std::fs::create_dir_all(d).unwrap();
        }
        let marks = bookmarks(&format!("file://{} Work\nsftp://host/x Remote\nfile://{}\n", mark.display(), docs.display()));
        assert_eq!(marks, [("Work".to_owned(), mark.clone()), ("Documents".to_owned(), docs.clone())]);
        let list = places(&tmp, &[docs.clone(), tmp.join("Missing")], &marks, &[("USB".into(), drive.clone())], true);
        let labels: Vec<&str> = list.iter().map(Place::label).collect();
        assert_eq!(labels, ["Recent", "Home", "Documents", "Work", "USB", "Computer"]);
        assert!(!places(&tmp, &[], &[], &[], false).contains(&Place::Recent));
        assert_eq!(label(&Row::Recent(PathBuf::from("/home/a/Documents/report.pdf")), ""), "report.pdf — Documents");
        let _ = std::fs::remove_dir_all(&tmp);
        let xbel = r#"<xbel><bookmark href="file:///a/b%20c.txt" added="2026-01-01T10:00:00Z" modified="2026-03-01T10:00:00.5Z" visited="x"><info/></bookmark>
<bookmark href="file:///x?a&amp;b" added="2026-02-01T00:00:00Z"></bookmark></xbel>"#;
        let e = xbel_entries(xbel);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].0, "file:///a/b%20c.txt");
        assert!(e[0].1 > e[1].1, "modified counts");
        assert_eq!(e[1].0, "file:///x?a&b");
    }

    #[test]
    fn globs_and_filters() {
        assert!(glob("*.png", "Photo.PNG"));
        assert!(glob("img-??.jpg", "img-01.jpg"));
        assert!(!glob("*.png", "notes.txt"));
        let images: Filter = ("Images".into(), vec![(0, "*.jpg".into()), (1, "image/png".into())]);
        assert!(passes(&images, "a.jpg"));
        assert!(passes(&images, "b.png"));
        assert!(!passes(&images, "c.txt"));
    }

    #[test]
    fn listings() {
        let entries: Vec<(String, bool)> = [("b.txt", false), ("Docs", true), (".hidden", true), ("a.png", false), ("apps", true)].iter().map(|(n, d)| (n.to_string(), *d)).collect();
        let dir = Path::new("/home/ada");
        let open = rows(dir, &entries, Mode::Open { multiple: false, directory: false }, None, false);
        assert_eq!(open, [Row::Parent, Row::Folder("apps".into()), Row::Folder("Docs".into()), Row::File("a.png".into()), Row::File("b.txt".into())]);
        let images: Filter = ("Images".into(), vec![(0, "*.png".into())]);
        let filtered = rows(dir, &entries, Mode::Open { multiple: true, directory: false }, Some(&images), true);
        assert_eq!(filtered[1], Row::FilterRow);
        assert_eq!(filtered.last(), Some(&Row::File("a.png".into())));
        let folders = rows(Path::new("/"), &entries, Mode::Open { multiple: false, directory: true }, Some(&images), true);
        assert_eq!(folders, [Row::SelectHere, Row::Folder("apps".into()), Row::Folder("Docs".into())]);
        let save = rows(dir, &entries, Mode::Save, None, false);
        assert_eq!(save[0], Row::SaveHere);
        assert_eq!(rows(dir, &entries, Mode::SaveFiles, None, true).len(), 4);
        assert_eq!(label(&Row::FilterRow, "Images"), "File type: Images (choose to change)");
    }

    #[test]
    fn paths() {
        let home = Path::new("/home/ada");
        assert_eq!(resolve(Path::new("/tmp"), "~/Pictures", home), Path::new("/home/ada/Pictures"));
        assert_eq!(resolve(Path::new("/tmp"), "/etc", home), Path::new("/etc"));
        assert_eq!(resolve(Path::new("/tmp"), "x/y.txt", home), Path::new("/tmp/x/y.txt"));
        assert_eq!(shown_path(Path::new("/home/ada/Pictures"), home), "~/Pictures");
        assert_eq!(shown_path(home, home), "~");
        assert_eq!(shown_path(Path::new("/srv"), home), "/srv");
        assert_eq!(path_of(b"/home/ada/a b.txt\0"), Path::new("/home/ada/a b.txt"));
    }
}
