//! The file chooser (helixdesktop/spec
//! `contracts/dbus/org.freedesktop.impl.portal.md`, "FileChooser",
//! "Helix"): what `org.helixdesktop.PortalDialog.ChooseFiles` asks for, and the
//! parts of the dialog that do not draw (rows, type-ahead, the path bar's
//! folders, the name field's editing, how sizes and dates read). The
//! dialog itself is `ui/file_chooser.rs`; it browses folders itself, so
//! one dialog stays up for the whole choice.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use helix_settings::files::{self, Entry, Filter, Place};
use tokio::sync::{mpsc, oneshot};
use zbus::zvariant::OwnedValue;

use crate::UiEvent;
use helix_i18n::tr;

/// What is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Files to open (one, or several with `multiple`).
    Open,
    /// A folder.
    Folder,
    /// A name to save a file under.
    Save,
    /// A folder to save files into.
    SaveFiles,
}

impl Mode {
    /// Files are listed (not only folders).
    pub fn lists_files(self) -> bool {
        matches!(self, Mode::Open | Mode::Save)
    }

    pub fn saves(self) -> bool {
        matches!(self, Mode::Save | Mode::SaveFiles)
    }
}

/// What the portal asks (`ChooseFiles` options).
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub title: String,
    pub accept: String,
    pub mode: Mode,
    pub multiple: bool,
    /// The folder to start in (`None`: Recent when opening files and
    /// `startup-mode` is `recent`, else Home).
    pub folder: Option<PathBuf>,
    /// The name to save under (Save).
    pub name: String,
    pub filters: Vec<Filter>,
    pub filter: Option<usize>,
    /// A check box: its label and state.
    pub check: Option<(String, bool)>,
    /// The sidebar's places (read once per dialog).
    pub places: Vec<Place>,
}

fn get<T: TryFrom<OwnedValue>>(options: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
    options.get(key).and_then(|v| T::try_from(v.clone()).ok())
}

/// A path from the bytes of an `ay` (a trailing NUL dropped).
pub fn path_of(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    PathBuf::from(std::ffi::OsString::from_vec(bytes[..end].to_vec()))
}

/// The options `ChooseFiles` was called with (unknown keys ignored).
pub fn options_from(options: &HashMap<String, OwnedValue>) -> Options {
    let text = |key: &str| get::<String>(options, key).unwrap_or_default();
    let mode = match text("mode").as_str() {
        "folder" => Mode::Folder,
        "save" => Mode::Save,
        "save-files" => Mode::SaveFiles,
        _ => Mode::Open,
    };
    let filters: Vec<Filter> = get(options, "filters").unwrap_or_default();
    let filter = get::<i32>(options, "filter").and_then(|i| usize::try_from(i).ok()).filter(|i| *i < filters.len());
    let check_label = text("check-label");
    let accept = text("accept");
    let accept = if accept.is_empty() {
        match mode {
            Mode::Open => tr("Open"),
            Mode::Folder => tr("Select"),
            Mode::Save | Mode::SaveFiles => tr("Save"),
        }
        .to_owned()
    } else {
        accept
    };
    let title = text("title");
    Options {
        title: if title.is_empty() { accept.clone() } else { title },
        accept,
        mode,
        multiple: mode == Mode::Open && get(options, "multiple").unwrap_or(false),
        folder: get::<Vec<u8>>(options, "folder").map(|b| path_of(&b)).filter(|p| p.is_absolute()),
        name: text("name"),
        filters,
        filter,
        check: (!check_label.is_empty()).then(|| (check_label, get(options, "check").unwrap_or(false))),
        places: Vec::new(),
    }
}

/// What the user chose.
#[derive(Debug, Clone, PartialEq)]
pub struct Chosen {
    pub paths: Vec<PathBuf>,
    /// The folder shown last.
    pub folder: PathBuf,
    pub filter: Option<usize>,
    pub check: bool,
}

/// A choice to make and where its answer goes (`None`: canceled).
pub struct Request {
    pub options: Options,
    pub reply: oneshot::Sender<Option<Chosen>>,
}

/// A [`Request`] that can travel in a [`UiEvent`].
#[derive(Clone)]
pub struct RequestSlot(Arc<Mutex<Option<Request>>>);

impl std::fmt::Debug for RequestSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequestSlot")
    }
}

impl PartialEq for RequestSlot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl RequestSlot {
    pub fn take(&self) -> Option<Request> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Show the file chooser and wait for the answer.
pub async fn ask(ui: &mpsc::UnboundedSender<UiEvent>, options: Options) -> Option<Chosen> {
    let (reply, answer) = oneshot::channel();
    let slot = RequestSlot(Arc::new(Mutex::new(Some(Request { options, reply }))));
    ui.send(UiEvent::ChooseFiles(slot)).ok()?;
    answer.await.ok().flatten()
}

/// What a list shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing {
    Folder(PathBuf),
    Recent,
    /// Files below `dir` matching `query` (and the filter, by name).
    Search { dir: PathBuf, query: String, filter: Option<Filter> },
}

/// How long a listing may take before the dialog says it is loading.
pub const LOADING_AFTER: std::time::Duration = std::time::Duration::from_millis(200);

/// List what `listing` names (blocking: run it on a worker thread).
/// With `HELIX_TEST_LIST_DELAY=<text>:<ms>`, folders whose path holds
/// `<text>` take that much longer (for tests of slow folders).
pub fn list_blocking(listing: &Listing, home: &Path) -> Result<Vec<Entry>, String> {
    match listing {
        Listing::Folder(dir) => {
            if let Some((needle, ms)) = std::env::var("HELIX_TEST_LIST_DELAY").ok().and_then(|v| v.rsplit_once(':').map(|(a, b)| (a.to_owned(), b.parse::<u64>().unwrap_or(0)))) {
                if dir.to_string_lossy().contains(&needle) {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
            }
            files::list(dir).map_err(|e| e.to_string())
        }
        Listing::Recent => Ok(files::recent_files(home)),
        Listing::Search { .. } => Err("searches are not blocking".into()),
    }
}

// ----- Rows -------------------------------------------------------------

/// The rows to show (indexes into `entries`), sorted: hidden entries only
/// with `hidden`; files only when the mode lists them and they pass
/// `filter`.
pub fn rows(entries: &[Entry], mode: Mode, filter: Option<&Filter>, hidden: bool, sort: files::Sort) -> Vec<usize> {
    rows_typed(entries, mode, filter, hidden, sort, TypeFormat::default())
}

/// [`rows`], sorting by type as the Type column shows it in `format`.
pub fn rows_typed(entries: &[Entry], mode: Mode, filter: Option<&Filter>, hidden: bool, sort: files::Sort, format: TypeFormat) -> Vec<usize> {
    let mut out: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| hidden || !e.hidden())
        .filter(|(_, e)| e.dir || (mode.lists_files() && filter.is_none_or(|f| files::passes(f, &e.name))))
        .map(|(i, _)| i)
        .collect();
    let types: Vec<String> = match sort.column {
        files::SortColumn::Type => entries.iter().map(|e| type_text(e, format)).collect(),
        _ => Vec::new(),
    };
    files::sort_rows_typed(entries, &mut out, sort, &types);
    out
}

/// How the Type column names types (`org.gtk.gtk4.Settings.FileChooser`
/// `type-format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TypeFormat {
    /// The MIME type (`image/png`).
    Mime,
    /// The type's description (`PNG image`).
    Description,
    /// A broad category (`Image`), as GTK 4's chooser shows by default.
    #[default]
    Category,
}

impl TypeFormat {
    pub fn from_key(key: &str) -> Self {
        match key {
            "mime" => TypeFormat::Mime,
            "description" => TypeFormat::Description,
            _ => TypeFormat::Category,
        }
    }
}

/// The category of a content type, from its generic icon, as GTK 4's
/// file chooser has them ("Unknown" for the rest).
fn category(content_type: &str) -> &'static str {
    let generic = gio::content_type_get_generic_icon_name(content_type).map(|g| g.to_string()).unwrap_or_default();
    match generic.as_str() {
        "application-x-executable" | "text-x-script" => tr("Program"),
        "audio-x-generic" => tr("Audio"),
        "font-x-generic" => tr("Font"),
        "image-x-generic" => tr("Image"),
        "package-x-generic" => tr("Archive"),
        "text-html" => tr("Markup"),
        "text-x-generic" | "text-x-generic-template" => tr("Text"),
        "video-x-generic" => tr("Video"),
        "x-office-address-book" => tr("Contacts"),
        "x-office-calendar" => tr("Calendar"),
        "x-office-document" => tr("Document"),
        "x-office-presentation" => tr("Presentation"),
        "x-office-spreadsheet" => tr("Spreadsheet"),
        // Translators: a file type the chooser cannot name.
        _ => tr("Unknown"),
    }
}

/// An entry's type as the Type column shows it ("Folder" for folders).
pub fn type_text(entry: &Entry, format: TypeFormat) -> String {
    if entry.dir {
        return match format {
            TypeFormat::Mime => "inode/directory".to_owned(),
            // Translators: the type of a folder in the file chooser.
            _ => tr("Folder").to_owned(),
        };
    }
    match format {
        TypeFormat::Mime => gio::content_type_get_mime_type(&entry.content_type).map(|m| m.to_string()).unwrap_or_else(|| entry.content_type.clone()),
        TypeFormat::Description => gio::content_type_get_description(&entry.content_type).to_string(),
        TypeFormat::Category => category(&entry.content_type).to_owned(),
    }
}

/// The first row (in shown order) whose name starts with `typed`,
/// ignoring case and accents; from `from` on, wrapping round.
pub fn typeahead(entries: &[Entry], rows: &[usize], typed: &str, from: usize) -> Option<usize> {
    let want = files::fold(typed);
    if want.is_empty() || rows.is_empty() {
        return None;
    }
    let n = rows.len();
    (0..n).map(|k| (from + k) % n).find(|&r| files::fold(&entries[rows[r]].name).starts_with(&want))
}

/// The path bar's buttons for `tail` (the deepest folder of the branch
/// shown; the folder shown is it or one of its parents): the home folder
/// (or `/`) and each folder below it, as (label, path); the first one's
/// label is empty (the UI names it).
pub fn crumbs(tail: &Path, home: &Path) -> Vec<(String, PathBuf)> {
    let (root, rest) = match tail.strip_prefix(home) {
        Ok(rest) => (home.to_path_buf(), rest.to_path_buf()),
        Err(_) => (PathBuf::from("/"), tail.strip_prefix("/").map(Path::to_path_buf).unwrap_or_default()),
    };
    let mut out = vec![(String::new(), root.clone())];
    let mut at = root;
    for part in rest.components() {
        at = at.join(part);
        out.push((part.as_os_str().to_string_lossy().into_owned(), at.clone()));
    }
    out
}

/// Where a file name's base ends (before its last extension), for
/// selecting it in the name field; the whole name when it has none (or
/// only a leading dot).
pub fn base_name_end(name: &str) -> usize {
    match name.rfind('.') {
        Some(0) | None => name.len(),
        Some(i) => i,
    }
}

/// A size as GNOME shows it (`g_format_size`: "1.2 MB", "340 bytes").
pub fn size_text(bytes: u64) -> String {
    gio::glib::format_size(bytes).to_string()
}

/// When a file was modified, as GNOME's file chooser shows it: the time
/// for today, "Yesterday", the day and month this year, else with the
/// year (local time; `now` and `t` Unix seconds).
pub fn modified_text(t: i64, now: i64, twelve_hour: bool, with_time: bool) -> String {
    if t <= 0 {
        return String::new();
    }
    let (Ok(when), Ok(now)) = (gio::glib::DateTime::from_unix_local(t), gio::glib::DateTime::from_unix_local(now)) else { return String::new() };
    let day = |d: &gio::glib::DateTime| (d.year(), d.day_of_year());
    let format = |d: &gio::glib::DateTime, f: &str| d.format(f).map(|s| s.to_string()).unwrap_or_default();
    // Translators: a time of day for today, 24-hour and 12-hour (see
    // GLib's g_date_time_format for the codes).
    let time = || format(&when, if twelve_hour { tr("%l:%M %p") } else { tr("%H:%M") }).trim().to_owned();
    if day(&when) == day(&now) {
        return time();
    }
    let yesterday = now.add_days(-1).ok();
    if yesterday.as_ref().is_some_and(|y| day(y) == day(&when)) {
        return if with_time {
            // Translators: yesterday at a time of day ("14:05", "2:05 PM").
            helix_i18n::trf!("Yesterday, {time}", time = time())
        } else {
            tr("Yesterday").to_owned()
        };
    }
    // `date-format` `with-time`: the time after the date.
    let pattern = match (when.year() == now.year(), with_time, twelve_hour) {
        // Translators: a date this year (day and abbreviated month).
        (true, false, _) => tr("%-e %b"),
        // Translators: a date this year with the time, 24-hour and 12-hour.
        (true, true, false) => tr("%-e %b, %H:%M"),
        (true, true, true) => tr("%-e %b, %l:%M %p"),
        // Translators: a date in another year.
        (false, false, _) => tr("%-e %b %Y"),
        // Translators: a date in another year with the time, 24-hour and
        // 12-hour.
        (false, true, false) => tr("%-e %b %Y, %H:%M"),
        (false, true, true) => tr("%-e %b %Y, %l:%M %p"),
    };
    // (`%l` pads with a space, or GLib's figure space.)
    format(&when, pattern).replace('\u{2007}', " ").replace("  ", " ").trim().to_owned()
}

// ----- The name field ---------------------------------------------------

/// A one-line text field's text, caret and selection (characters, not
/// bytes).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextEdit {
    pub text: String,
    /// The caret, in characters.
    pub caret: usize,
    /// Where the selection started (the caret is its other end); `None`
    /// or equal to the caret: nothing selected.
    pub anchor: Option<usize>,
}

impl TextEdit {
    pub fn new(text: &str) -> Self {
        Self { text: text.to_owned(), caret: text.chars().count(), anchor: None }
    }

    /// With characters `from..to` selected (the caret at `to`).
    pub fn selecting(text: &str, from: usize, to: usize) -> Self {
        Self { text: text.to_owned(), caret: to, anchor: Some(from) }
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn byte(&self, chars: usize) -> usize {
        self.text.char_indices().nth(chars).map_or(self.text.len(), |(i, _)| i)
    }

    /// The selected characters' range, if any.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        (a != self.caret).then(|| (a.min(self.caret), a.max(self.caret)))
    }

    fn delete_selection(&mut self) -> bool {
        let Some((a, b)) = self.selection() else { return false };
        let (ba, bb) = (self.byte(a), self.byte(b));
        self.text.replace_range(ba..bb, "");
        self.caret = a;
        self.anchor = None;
        true
    }

    /// Type `s` (replacing the selection).
    pub fn insert(&mut self, s: &str) {
        let s: String = s.chars().filter(|c| !c.is_control()).collect();
        if s.is_empty() {
            return;
        }
        self.delete_selection();
        let at = self.byte(self.caret);
        self.text.insert_str(at, &s);
        self.caret += s.chars().count();
    }

    pub fn backspace(&mut self) {
        if !self.delete_selection() && self.caret > 0 {
            let (a, b) = (self.byte(self.caret - 1), self.byte(self.caret));
            self.text.replace_range(a..b, "");
            self.caret -= 1;
        }
    }

    pub fn delete(&mut self) {
        if !self.delete_selection() && self.caret < self.len() {
            let (a, b) = (self.byte(self.caret), self.byte(self.caret + 1));
            self.text.replace_range(a..b, "");
        }
    }

    /// Move the caret to `to` (with `extend`: growing the selection).
    pub fn move_to(&mut self, to: usize, extend: bool) {
        let to = to.min(self.len());
        if extend {
            self.anchor.get_or_insert(self.caret);
        } else {
            self.anchor = None;
        }
        self.caret = to;
    }

    /// Left (`-1`) or right (`1`); without `extend` a selection collapses
    /// to its end on that side.
    pub fn step(&mut self, by: isize, extend: bool) {
        if !extend {
            if let Some((a, b)) = self.selection() {
                self.move_to(if by < 0 { a } else { b }, false);
                return;
            }
        }
        let to = self.caret.saturating_add_signed(by);
        self.move_to(to, extend);
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.caret = self.len();
    }

    /// The text before the caret (for placing it).
    pub fn before_caret(&self) -> &str {
        &self.text[..self.byte(self.caret)]
    }

    /// The text before character `at`.
    pub fn before(&self, at: usize) -> &str {
        &self.text[..self.byte(at)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_settings::files::{Sort, SortColumn};

    fn entry(name: &str, dir: bool) -> Entry {
        Entry { name: name.into(), path: PathBuf::from("/d").join(name), dir, size: name.len() as u64, modified: 0, content_type: files::fast_content_type(name) }
    }

    #[test]
    fn rows_and_typeahead() {
        let entries: Vec<Entry> = [("b.txt", false), ("Invoices", true), (".hidden-config", false), ("a.png", false), ("Archive", true), ("invoice-2.pdf", false), ("invoice-10.pdf", false)].iter().map(|(n, d)| entry(n, *d)).collect();
        let names = |rows: &[usize]| rows.iter().map(|i| entries[*i].name.clone()).collect::<Vec<_>>();
        let open = rows(&entries, Mode::Open, None, false, Sort::default());
        assert_eq!(names(&open), ["Archive", "Invoices", "a.png", "b.txt", "invoice-2.pdf", "invoice-10.pdf"]);
        assert!(names(&rows(&entries, Mode::Open, None, true, Sort::default())).contains(&".hidden-config".to_owned()));
        let pdf: Filter = ("PDF".into(), vec![(0, "*.pdf".into())]);
        assert_eq!(names(&rows(&entries, Mode::Open, Some(&pdf), false, Sort::default())), ["Archive", "Invoices", "invoice-2.pdf", "invoice-10.pdf"], "folders always");
        assert_eq!(names(&rows(&entries, Mode::Folder, None, false, Sort::default())), ["Archive", "Invoices"]);
        assert_eq!(names(&rows(&entries, Mode::SaveFiles, None, false, Sort::default())), ["Archive", "Invoices"]);
        let by_size = rows(&entries, Mode::Open, None, false, Sort { column: SortColumn::Size, ascending: true, folders_first: true });
        assert_eq!(names(&by_size)[2..4], ["a.png".to_owned(), "b.txt".to_owned()]);
        // Type-ahead: case and accents ignored, wrapping from the row.
        assert_eq!(typeahead(&entries, &open, "inv", 0), Some(1));
        assert_eq!(typeahead(&entries, &open, "INVO", 2), Some(4), "the next match from row 2");
        assert_eq!(typeahead(&entries, &open, "ínv", 0), Some(1), "accents ignored");
        assert_eq!(typeahead(&entries, &open, "arc", 3), Some(0), "wraps round");
        assert_eq!(typeahead(&entries, &open, "zzz", 0), None);
    }

    #[test]
    fn path_bar() {
        let home = Path::new("/home/ada");
        let c = crumbs(Path::new("/home/ada/Documents/Client — Contoso Ltd/2026"), home);
        let labels: Vec<&str> = c.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["", "Documents", "Client — Contoso Ltd", "2026"]);
        assert_eq!(c[1].1, Path::new("/home/ada/Documents"));
        assert_eq!(c[0].1, home);
        let c = crumbs(Path::new("/srv/data"), home);
        assert_eq!(c.iter().map(|(l, p)| (l.as_str(), p.to_str().unwrap())).collect::<Vec<_>>(), [("", "/"), ("srv", "/srv"), ("data", "/srv/data")]);
        assert_eq!(crumbs(Path::new("/"), home).len(), 1);
    }

    #[test]
    fn name_field() {
        assert_eq!(base_name_end("Untitled document.odt"), 17);
        assert_eq!(base_name_end("archive.tar.gz"), 11);
        assert_eq!(base_name_end(".bashrc"), 7);
        assert_eq!(base_name_end("README"), 6);
        // The base name selected: typing replaces it, the extension stays.
        let mut e = TextEdit::selecting("Untitled document.odt", 0, base_name_end("Untitled document.odt"));
        e.insert("Q");
        e.insert("3 report");
        assert_eq!((e.text.as_str(), e.caret), ("Q3 report.odt", 9));
        e.backspace();
        e.step(1, false);
        e.delete();
        assert_eq!(e.text, "Q3 repor.dt");
        e.move_to(0, false);
        e.step(2, true);
        assert_eq!(e.selection(), Some((0, 2)));
        e.step(-1, false);
        assert_eq!((e.caret, e.selection()), (0, None), "collapses to the start");
        e.select_all();
        e.backspace();
        assert_eq!(e.text, "");
        let mut u = TextEdit::new("日本語.txt");
        u.step(-4, false);
        u.backspace();
        assert_eq!(u.text, "日本.txt");
        assert_eq!(u.before_caret(), "日本");
    }

    #[test]
    fn options() {
        use zbus::zvariant::Value;
        let v = |x: Value<'_>| OwnedValue::try_from(x).unwrap();
        let o = HashMap::from([
            ("mode".to_owned(), v(Value::from("save"))),
            ("folder".to_owned(), v(Value::from(b"/home/ada/Documents".to_vec()))),
            ("name".to_owned(), v(Value::from("Untitled document.odt"))),
            ("filters".to_owned(), v(Value::from(vec![("PDF".to_owned(), vec![(0u32, "*.pdf".to_owned())])]))),
            ("filter".to_owned(), v(Value::from(3i32))),
        ]);
        let o = options_from(&o);
        assert_eq!((o.mode, o.folder.as_deref(), o.name.as_str(), o.filter, o.title.as_str()), (Mode::Save, Some(Path::new("/home/ada/Documents")), "Untitled document.odt", None, "Save"));
        let o = options_from(&HashMap::from([("multiple".to_owned(), v(Value::from(true))), ("folder".to_owned(), v(Value::from(b"relative".to_vec())))]));
        assert_eq!((o.mode, o.multiple, o.folder), (Mode::Open, true, None));
    }

    #[test]
    fn types() {
        let file = |name: &str| Entry { name: name.into(), path: PathBuf::from("/x").join(name), dir: false, size: 1, modified: 0, content_type: files::fast_content_type(name) };
        let folder = Entry { dir: true, content_type: "inode/directory".into(), ..file("Docs") };
        assert_eq!(type_text(&folder, TypeFormat::Category), "Folder");
        assert_eq!(type_text(&folder, TypeFormat::Mime), "inode/directory");
        // Without shared-mime-info nothing is known; with it, as GTK names them.
        if gio::content_type_get_mime_type(&files::fast_content_type("a.png")).is_some_and(|m| m == "image/png") {
            assert_eq!(type_text(&file("a.png"), TypeFormat::Category), "Image");
            assert_eq!(type_text(&file("a.png"), TypeFormat::Mime), "image/png");
            assert_eq!(type_text(&file("notes.txt"), TypeFormat::Category), "Text");
            assert!(!type_text(&file("a.png"), TypeFormat::Description).is_empty());
            // Sorted by the type shown: folders, then Image before Text.
            let entries = vec![file("notes.txt"), file("a.png"), folder.clone()];
            let sort = files::Sort { column: files::SortColumn::Type, ascending: true, folders_first: false };
            assert_eq!(rows_typed(&entries, Mode::Open, None, false, sort, TypeFormat::Category), [2, 1, 0]);
        }
        assert_eq!((TypeFormat::from_key("mime"), TypeFormat::from_key("description"), TypeFormat::from_key("?")), (TypeFormat::Mime, TypeFormat::Description, TypeFormat::Category));
    }

    #[test]
    fn dates_and_sizes() {
        let now = gio::glib::DateTime::from_local(2026, 11, 16, 15, 30, 0.0).unwrap().to_unix();
        let at = |d: i32, h: i32| gio::glib::DateTime::from_local(2026, 11, d, h, 5, 0.0).unwrap().to_unix();
        assert_eq!(modified_text(at(16, 9), now, false, false), "09:05");
        assert_eq!(modified_text(at(16, 21), now, true, false), "9:05 PM");
        assert_eq!(modified_text(at(15, 23), now, false, false), "Yesterday");
        assert_eq!(modified_text(at(2, 10), now, false, false), "2 Nov");
        let old = gio::glib::DateTime::from_local(2025, 3, 7, 10, 0, 0.0).unwrap().to_unix();
        assert_eq!(modified_text(old, now, false, false), "7 Mar 2025");
        assert_eq!(modified_text(0, now, false, false), "");
        // `date-format` `with-time`.
        assert_eq!(modified_text(at(16, 9), now, false, true), "09:05", "today: the time alone");
        assert_eq!(modified_text(at(15, 23), now, false, true), "Yesterday, 23:05");
        assert_eq!(modified_text(at(2, 10), now, false, true), "2 Nov, 10:05");
        assert_eq!(modified_text(at(2, 9), now, true, true), "2 Nov, 9:05 AM");
        assert_eq!(modified_text(old, now, false, true), "7 Mar 2025, 10:00");
        assert_eq!(size_text(340), "340 bytes");
        assert_eq!(size_text(1_200_000).replace('\u{a0}', " "), "1.2 MB", "(GLib puts a no-break space before the unit)");
    }
}
