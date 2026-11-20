//! What the file chooser lists (helixdesktop/spec
//! `contracts/dbus/org.freedesktop.impl.portal.md`, "FileChooser"):
//! folders, places, recently used files and search results.
//!
//! Listing a folder reads its entries' metadata only, never their
//! contents: a file's type comes from its name (as GTK's
//! `standard::fast-content-type`), so unreadable files are listed and a
//! slow drive is not read file by file. Everything here blocks; callers
//! run it off their async executors.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use gio::prelude::*;

/// A filter: its name and patterns, (0, glob) or (1, MIME type).
pub type Filter = (String, Vec<(u32, String)>);

/// Glob matching as GLib's `GPatternSpec`-style file filters use it,
/// ignoring case: `*`, `?` and bracket classes (`[aA]`, `[0-9]`, `[!x]` or
/// `[^x]`). Apps make case-insensitive filters with classes: Chromium's
/// file dialog and GTK's `gtk_file_filter_add_suffix` send `*.[pP][dD][fF]`.
/// A `[` with no closing `]` matches itself.
pub fn glob(pattern: &str, name: &str) -> bool {
    /// The class at the start of `p` (just after its `[`): whether `c`
    /// is in it, and how many chars the class used up (with its `]`).
    fn class(p: &[char], c: char) -> Option<(bool, usize)> {
        let mut i = 0;
        let negate = matches!(p.first(), Some('!' | '^'));
        if negate {
            i += 1;
        }
        let mut found = false;
        let mut first = true;
        while i < p.len() {
            let a = p[i];
            if a == ']' && !first {
                return Some((found != negate, i + 1));
            }
            first = false;
            if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
                if a <= c && c <= p[i + 2] {
                    found = true;
                }
                i += 3;
            } else {
                if a == c {
                    found = true;
                }
                i += 1;
            }
        }
        None
    }
    fn go(p: &[char], n: &[char]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some(('*', rest)) => (0..=n.len()).any(|i| go(rest, &n[i..])),
            Some(('?', rest)) => !n.is_empty() && go(rest, &n[1..]),
            Some(('[', rest)) => match n.first().and_then(|c| class(rest, *c).map(|r| (c, r))) {
                Some((_, (true, used))) => go(&rest[used..], &n[1..]),
                Some((_, (false, _))) => false,
                // No closing bracket (or no name left): a literal `[`.
                None => !rest.contains(&']') && n.first() == Some(&'[') && go(rest, &n[1..]),
            },
            Some((c, rest)) => n.first().is_some_and(|m| m == c) && go(rest, &n[1..]),
        }
    }
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    go(&p, &n)
}

/// The content type guessed from a file name alone.
pub fn fast_content_type(name: &str) -> String {
    gio::content_type_guess(Some(name), None).0.to_string()
}

/// Whether file `name` passes `filter` (globs ignore case; MIME types
/// by the type guessed from the name).
pub fn passes(filter: &Filter, name: &str) -> bool {
    filter.1.iter().any(|(kind, pattern)| match kind {
        0 => glob(pattern, name),
        _ => gio::content_type_is_mime_type(&fast_content_type(name), pattern),
    })
}

/// A name's sort key: case and accents folded, runs of digits compared
/// by value (`file2` before `file10`, as `g_utf8_collate_key_for_filename`
/// orders them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameKey(Vec<KeyPart>);

#[derive(Debug, Clone, PartialEq, Eq)]
enum KeyPart {
    /// Digits: their value without leading zeros (as text, so any length
    /// compares), then how many leading zeros there were.
    Number(String, usize),
    Text(String),
}

impl NameKey {
    pub fn new(name: &str) -> Self {
        // Decomposed, so accents are separate marks, then dropped.
        let folded: String = gio::glib::normalize(gio::glib::casefold(name), gio::glib::NormalizeMode::All)
            .chars()
            .filter(|c| !matches!(*c as u32, 0x300..=0x36F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F))
            .collect();
        let mut parts = Vec::new();
        let mut chars = folded.chars().peekable();
        while let Some(&c) = chars.peek() {
            let digit = c.is_ascii_digit();
            let mut run = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() != digit {
                    break;
                }
                run.push(d);
                chars.next();
            }
            if digit {
                let value = run.trim_start_matches('0');
                parts.push(KeyPart::Number(value.to_owned(), run.len() - value.len()));
            } else {
                parts.push(KeyPart::Text(run));
            }
        }
        Self(parts)
    }
}

impl Ord for KeyPart {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (KeyPart::Number(a, za), KeyPart::Number(b, zb)) => a.len().cmp(&b.len()).then_with(|| a.cmp(b)).then_with(|| za.cmp(zb)),
            // Numbers before text, as digits come before letters.
            (KeyPart::Number(..), KeyPart::Text(_)) => Ordering::Less,
            (KeyPart::Text(_), KeyPart::Number(..)) => Ordering::Greater,
            (KeyPart::Text(a), KeyPart::Text(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for KeyPart {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NameKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

impl PartialOrd for NameKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Natural order of two names (see [`NameKey`]); names that fold alike
/// keep a stable order by their bytes.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    NameKey::new(a).cmp(&NameKey::new(b)).then_with(|| a.cmp(b))
}

/// The folded form type-ahead compares against (case and accents
/// folded, as [`NameKey`] does).
pub fn fold(text: &str) -> String {
    gio::glib::normalize(gio::glib::casefold(text), gio::glib::NormalizeMode::All)
        .chars()
        .filter(|c| !matches!(*c as u32, 0x300..=0x36F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F))
        .collect()
}

/// A listed file or folder.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub dir: bool,
    /// Bytes (0 for folders).
    pub size: u64,
    /// Last modified, Unix seconds (0: unknown).
    pub modified: i64,
    /// The content type from the name (`inode/directory` for folders).
    pub content_type: String,
    /// A trashed item: the folder it was in before.
    pub origin: Option<PathBuf>,
}

impl Entry {
    /// The entry for `path` from its metadata (`None` when it is gone).
    /// A link is followed; a broken one is listed as a file.
    pub fn at(path: PathBuf) -> Option<Self> {
        let name = path.file_name()?.to_string_lossy().into_owned();
        let meta = std::fs::metadata(&path).or_else(|_| std::fs::symlink_metadata(&path)).ok()?;
        let dir = meta.is_dir();
        let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64);
        let content_type = if dir { "inode/directory".to_owned() } else { fast_content_type(&name) };
        Some(Self { name, path, dir, size: if dir { 0 } else { meta.len() }, modified, content_type, origin: None })
    }

    pub fn hidden(&self) -> bool {
        self.name.starts_with('.') || self.name.ends_with('~')
    }
}

/// The entries of folder `dir`, unsorted; their metadata only (nothing
/// is opened but the folder).
pub fn list(dir: &Path) -> std::io::Result<Vec<Entry>> {
    Ok(std::fs::read_dir(dir)?.flatten().filter_map(|e| Entry::at(e.path())).collect())
}

/// What a list is sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortColumn {
    #[default]
    Name,
    Size,
    Modified,
    /// The folder holding each file (Recent and search results).
    Location,
    /// The file's type as the Type column shows it.
    Type,
}

impl SortColumn {
    /// As `org.gtk.gtk4.Settings.FileChooser` `sort-column` names it
    /// (Location is stored as `name`).
    pub fn key(self) -> &'static str {
        match self {
            SortColumn::Name | SortColumn::Location => "name",
            SortColumn::Size => "size",
            SortColumn::Modified => "modified",
            SortColumn::Type => "type",
        }
    }

    pub fn from_key(key: &str) -> Self {
        match key {
            "size" => SortColumn::Size,
            "modified" => SortColumn::Modified,
            "type" => SortColumn::Type,
            _ => SortColumn::Name,
        }
    }
}

/// How a list is sorted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub column: SortColumn,
    pub ascending: bool,
    /// Folders before files whatever the column.
    pub folders_first: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self { column: SortColumn::Name, ascending: true, folders_first: true }
    }
}

/// Order `rows` (indexes into `entries`) by `sort`; ties by name. By
/// type: by content type (see [`sort_rows_typed`]).
pub fn sort_rows(entries: &[Entry], rows: &mut [usize], sort: Sort) {
    sort_rows_typed(entries, rows, sort, &[]);
}

/// [`sort_rows`], sorting by type by `types` (each entry's type as shown,
/// in `entries`' order; when empty, the content types).
pub fn sort_rows_typed(entries: &[Entry], rows: &mut [usize], sort: Sort, types: &[String]) {
    let mut keys = SortKeys::default();
    keys.extend(entries, sort, types);
    rows.sort_by(|&a, &b| keys.order(entries, sort, a, b));
}

/// Entries' sort keys, worked out once per entry (so a list that grows
/// in batches is not keyed again): the names', and the types' when
/// sorting by type.
#[derive(Debug, Clone, Default)]
pub struct SortKeys {
    names: Vec<NameKey>,
    /// Empty unless sorting by type (or by location: the folders').
    other: Vec<NameKey>,
    /// What `other` holds.
    column: Option<SortColumn>,
}

impl SortKeys {
    /// Key the entries not keyed yet (`types` as in [`sort_rows_typed`]).
    /// A sort by another column than the keys were made for keys again.
    pub fn extend(&mut self, entries: &[Entry], sort: Sort, types: &[String]) {
        let wanted = matches!(sort.column, SortColumn::Type | SortColumn::Location).then_some(sort.column);
        if wanted != self.column {
            self.other.clear();
            self.column = wanted;
        }
        self.names.extend(entries[self.names.len().min(entries.len())..].iter().map(|e| NameKey::new(&e.name)));
        let from = self.other.len();
        match wanted {
            Some(SortColumn::Type) => self.other.extend(entries.iter().enumerate().skip(from).map(|(i, e)| NameKey::new(types.get(i).unwrap_or(&e.content_type)))),
            Some(_) => self.other.extend(entries.iter().skip(from).map(|e| NameKey::new(&e.origin.as_deref().or_else(|| e.path.parent()).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()))),
            None => {}
        }
    }

    /// Forget every key (the entries were replaced).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// How entries `a` and `b` are ordered by `sort` (keyed with
    /// [`Self::extend`] for this sort); ties by name.
    pub fn order(&self, entries: &[Entry], sort: Sort, a: usize, b: usize) -> Ordering {
        let (ea, eb) = (&entries[a], &entries[b]);
        if sort.folders_first && ea.dir != eb.dir {
            return eb.dir.cmp(&ea.dir);
        }
        let by_name = || self.names[a].cmp(&self.names[b]).then_with(|| ea.name.cmp(&eb.name));
        let order = match sort.column {
            SortColumn::Name => by_name(),
            SortColumn::Size => ea.size.cmp(&eb.size).then_with(by_name),
            SortColumn::Modified => ea.modified.cmp(&eb.modified).then_with(by_name),
            SortColumn::Location | SortColumn::Type => self.other[a].cmp(&self.other[b]).then_with(by_name),
        };
        if sort.ascending { order } else { order.reverse() }
    }
}

/// Merge `new` rows (sorted by `order`) into `rows` (sorted alike),
/// keeping the order: one pass, for lists that grow in batches.
pub fn merge_rows(rows: &mut Vec<usize>, new: Vec<usize>, order: impl Fn(usize, usize) -> Ordering) {
    if new.is_empty() {
        return;
    }
    let old = std::mem::take(rows);
    rows.reserve(old.len() + new.len());
    let (mut a, mut b) = (old.into_iter().peekable(), new.into_iter().peekable());
    loop {
        match (a.peek(), b.peek()) {
            (Some(&x), Some(&y)) => {
                if order(y, x) == Ordering::Less {
                    rows.push(y);
                    b.next();
                } else {
                    rows.push(x);
                    a.next();
                }
            }
            (Some(_), None) => rows.extend(a.by_ref()),
            (None, Some(_)) => rows.extend(b.by_ref()),
            (None, None) => break,
        }
    }
}

// ----- Places -----------------------------------------------------------

/// What a place is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceKind {
    Recent,
    /// GNOME Files' starred files (`crate::stars`).
    Starred,
    Home,
    /// A standard folder (Desktop, Documents, …).
    Standard,
    Bookmark,
    Drive,
    Network,
    /// The whole computer (`/`).
    Computer,
    /// The Trash (its `path`: the home Trash's `files` folder).
    Trash,
    /// Other Locations: the computer, its drives and volumes.
    Other,
}

/// A place in the chooser's sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub kind: PlaceKind,
    /// The folder's or bookmark's name (empty for Recent, Starred,
    /// Home, Computer, Trash and Other Locations, which the UI names).
    pub label: String,
    /// `None` for Recent, Starred and Other Locations.
    pub path: Option<PathBuf>,
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

/// The places: Recent (when `recent`), Starred (when `starred`), Home,
/// the user's standard folders, the Trash (when `trash`, at its folder),
/// bookmarks, drives, network places, the whole computer and Other
/// Locations; folders that do not exist are left out, and none twice.
#[allow(clippy::too_many_arguments)]
pub fn places(home: &Path, standard: &[PathBuf], bookmarks: &[(String, PathBuf)], drives: &[(String, PathBuf)], network: &[(String, PathBuf)], recent: bool, starred: bool, trash: Option<&Path>) -> Vec<Place> {
    let mut out = Vec::new();
    if recent {
        out.push(Place { kind: PlaceKind::Recent, label: String::new(), path: None });
    }
    if starred {
        out.push(Place { kind: PlaceKind::Starred, label: String::new(), path: None });
    }
    out.push(Place { kind: PlaceKind::Home, label: String::new(), path: Some(home.to_path_buf()) });
    let named = |p: &PathBuf| (p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned()), p.clone());
    let seen = |out: &[Place], path: &Path| out.iter().any(|p| p.path.as_deref() == Some(path));
    for (label, path) in standard.iter().map(named) {
        if !seen(&out, &path) && path.is_dir() {
            out.push(Place { kind: PlaceKind::Standard, label, path: Some(path) });
        }
    }
    // The Trash, whether or not its folder is there yet (an empty one).
    if let Some(trash) = trash {
        out.push(Place { kind: PlaceKind::Trash, label: String::new(), path: Some(trash.to_path_buf()) });
    }
    let candidates = bookmarks
        .iter()
        .map(|b| (PlaceKind::Bookmark, b.clone()))
        .chain(drives.iter().map(|d| (PlaceKind::Drive, d.clone())))
        .chain(network.iter().map(|d| (PlaceKind::Network, d.clone())))
        .chain([(PlaceKind::Computer, (String::new(), PathBuf::from("/")))]);
    for (kind, (label, path)) in candidates {
        if !seen(&out, &path) && path.is_dir() {
            out.push(Place { kind, label, path: Some(path) });
        }
    }
    out.push(Place { kind: PlaceKind::Other, label: String::new(), path: None });
    out
}

/// The standard folders (Desktop, Documents, Downloads, Music, Pictures,
/// Videos) that are not the home folder itself.
pub fn standard_folders(home: &Path) -> Vec<PathBuf> {
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

/// A readable name for a GVfs FUSE folder (`smb-share:server=nas,share=docs`
/// is "docs on nas"; `sftp:host=box,user=me` "me on box"); else the name.
pub fn gvfs_label(name: &str) -> String {
    let Some((kind, args)) = name.split_once(':') else { return name.to_owned() };
    let get = |k: &str| args.split(',').find_map(|kv| kv.strip_prefix(k).and_then(|v| v.strip_prefix('='))).map(str::to_owned);
    match kind {
        "smb-share" => match (get("share"), get("server")) {
            (Some(share), Some(server)) => format!("{share} on {server}"),
            _ => name.to_owned(),
        },
        _ => match (get("host"), get("user")) {
            (Some(host), Some(user)) => format!("{user} on {host}"),
            (Some(host), None) => host,
            _ => name.to_owned(),
        },
    }
}

/// Network places the user connected to: GVfs's FUSE folders under
/// `$XDG_RUNTIME_DIR/gvfs`.
fn network_places() -> Vec<(String, PathBuf)> {
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else { return Vec::new() };
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(Path::new(&runtime).join("gvfs"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| (gvfs_label(&e.file_name().to_string_lossy()), e.path()))
        .collect();
    out.sort();
    out
}

/// The user's places (see [`places`]): with Recent when `recent`, and
/// then Starred when GNOME Files' star store is there; with the Trash
/// when `trash`.
pub fn user_places(home: &Path, recent: bool, trash: bool) -> Vec<Place> {
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    let marks = std::fs::read_to_string(config.join("gtk-3.0/bookmarks")).map(|t| bookmarks(&t)).unwrap_or_default();
    let trash = trash.then(|| home_trash(home).join("files"));
    let starred = recent && crate::stars::exists(&crate::stars::data_home());
    places(home, &standard_folders(home), &marks, &drives(), &network_places(), recent, starred, trash.as_deref())
}

/// GNOME Files' starred files inside `home` that are there, read-only
/// (`crate::stars`); an error when its store cannot be read.
pub fn starred_files(home: &Path) -> Result<Vec<Entry>, String> {
    let paths = crate::stars::starred_paths(&crate::stars::data_home(), home).map_err(|e| format!("Starred files cannot be read: {e}"))?;
    Ok(paths.into_iter().filter_map(Entry::at).collect())
}

// ----- The Trash ----------------------------------------------------------

/// The home Trash (`$XDG_DATA_HOME/Trash`).
pub fn home_trash(home: &Path) -> PathBuf {
    std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share")).join("Trash")
}

/// The Trash folders of a drive mounted at `top` for user `uid`, as the
/// freedesktop Trash specification has them: `$top/.Trash/$uid` (when
/// `.Trash` is a sticky folder, not a link) and `$top/.Trash-$uid`.
pub fn drive_trashes(top: &Path, uid: u32) -> Vec<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = Vec::new();
    let shared = top.join(".Trash");
    let sticky = std::fs::symlink_metadata(&shared).is_ok_and(|m| m.is_dir() && m.permissions().mode() & 0o1000 != 0);
    if sticky && shared.join(uid.to_string()).is_dir() {
        out.push(shared.join(uid.to_string()));
    }
    let own = top.join(format!(".Trash-{uid}"));
    if std::fs::symlink_metadata(&own).is_ok_and(|m| m.is_dir()) {
        out.push(own);
    }
    out
}

/// What a Trash folder (`trash`, holding `info` and `files`) holds: each
/// item with a `.trashinfo`, under its name before it was trashed, with
/// the folder it was in as its `origin` (a relative `Path=` is below
/// `top`, the drive's folder). The items themselves are listed (their
/// metadata only), so choosing one gives a file that can be read.
pub fn trashed(trash: &Path, top: Option<&Path>) -> Vec<Entry> {
    let Ok(infos) = std::fs::read_dir(trash.join("info")) else { return Vec::new() };
    infos
        .flatten()
        .filter_map(|info| {
            let info_path = info.path();
            if info_path.extension().is_none_or(|e| e != "trashinfo") {
                return None;
            }
            let stored = info_path.file_stem()?;
            let mut entry = Entry::at(trash.join("files").join(stored))?;
            let text = std::fs::read_to_string(&info_path).unwrap_or_default();
            let original = text.lines().find_map(|l| l.trim().strip_prefix("Path=")).map(|p| gio::glib::uri_unescape_string(p.trim(), None::<&str>).map_or_else(|| p.trim().to_owned(), |s| s.to_string()));
            if let Some(original) = original.map(PathBuf::from) {
                let original = match top {
                    Some(top) if original.is_relative() => top.join(original),
                    _ => original,
                };
                if let Some(name) = original.file_name() {
                    entry.name = name.to_string_lossy().into_owned();
                    if !entry.dir {
                        entry.content_type = fast_content_type(&entry.name);
                    }
                }
                entry.origin = original.parent().map(Path::to_path_buf);
            }
            Some(entry)
        })
        .collect()
}

/// Everything in the user's Trash: the home Trash, then the Trash
/// folders of the drives mounted at `tops`.
pub fn trash_entries(home: &Path, tops: &[PathBuf]) -> Vec<Entry> {
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata("/proc/self").map_or(u32::MAX, |m| m.uid());
    let mut out = trashed(&home_trash(home), None);
    for top in tops {
        for t in drive_trashes(top, uid) {
            out.extend(trashed(&t, Some(top)));
        }
    }
    out
}

// ----- Recent files -----------------------------------------------------

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

/// The oldest recent file to list (Unix seconds), or `None` to list none,
/// from `org.gnome.desktop.privacy` `remember-recent-files` and
/// `recent-files-max-age` (days; -1: no limit).
pub fn recent_cutoff(remember: bool, max_age_days: i32, now: i64) -> Option<i64> {
    match (remember, max_age_days) {
        (false, _) | (_, 0) => None,
        (true, days) if days < 0 => Some(i64::MIN),
        (true, days) => Some(now - i64::from(days) * 86_400),
    }
}

/// The oldest use of a recent file still shown (Unix seconds), as the
/// privacy settings say; `None` when recent files are not remembered.
pub fn privacy_cutoff() -> Option<i64> {
    const PRIVACY: &str = "org.gnome.desktop.privacy";
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let Some(schema) = gio::SettingsSchemaSource::default().and_then(|src| src.lookup(PRIVACY, true)) else { return recent_cutoff(true, -1, now) };
    let s = gio::Settings::new(PRIVACY);
    let remember = !schema.has_key("remember-recent-files") || s.boolean("remember-recent-files");
    let age = if schema.has_key("recent-files-max-age") { s.int("recent-files-max-age") } else { -1 };
    recent_cutoff(remember, age, now)
}

/// Recently used local files that still exist, newest first
/// (`recently-used.xbel` under the data folder), at most
/// [`RECENT_SHOWN`]; each with when it was used as its `modified`.
pub fn recent_files(home: &Path) -> Vec<Entry> {
    let Some(cutoff) = privacy_cutoff() else { return Vec::new() };
    let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share"));
    let Ok(text) = std::fs::read_to_string(data.join("recently-used.xbel")) else { return Vec::new() };
    let mut found: Vec<(i64, PathBuf)> = xbel_entries(&text)
        .into_iter()
        .filter_map(|(uri, when)| {
            let path = gio::File::for_uri(&uri).path().filter(|_| uri.starts_with("file://"))?;
            (when >= cutoff).then_some((when, path))
        })
        .collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.dedup_by(|a, b| a.1 == b.1);
    found
        .into_iter()
        .filter_map(|(when, path)| Entry::at(path).filter(|e| !e.dir).map(|e| Entry { modified: when, ..e }))
        .take(RECENT_SHOWN)
        .collect()
}

// ----- Search -----------------------------------------------------------

/// The most search results shown, and the most entries looked at.
pub const SEARCH_SHOWN: usize = 200;
const SEARCH_LOOKED: usize = 20_000;

/// Text files whose content a search reads, at most, and how big one may
/// be.
const CONTENT_READS: usize = 500;
const CONTENT_MAX: u64 = 256 * 1024;

/// Files under `dir` (its folders too, hidden ones skipped) whose names
/// hold `query` (ignoring case) and pass `filter`, nearest first, then by
/// name; then text files (at most 256 KiB, the first 500 looked into)
/// whose content holds it.
pub fn search(dir: &Path, query: &str, filter: Option<&Filter>) -> Vec<PathBuf> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut by_content = Vec::new();
    let mut reads = 0;
    let mut queue = std::collections::VecDeque::from([dir.to_path_buf()]);
    let mut looked = 0;
    'walk: while let Some(folder) = queue.pop_front() {
        let Ok(read) = std::fs::read_dir(&folder) else { continue };
        let mut names: Vec<(String, PathBuf, bool)> = read
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                (!name.starts_with('.')).then(|| (name, e.path(), e.file_type().is_ok_and(|t| t.is_dir())))
            })
            .collect();
        names.sort_by_key(|(n, _, _)| n.to_lowercase());
        for (name, path, is_dir) in names {
            looked += 1;
            if looked > SEARCH_LOOKED || found.len() >= SEARCH_SHOWN {
                break 'walk;
            }
            if is_dir {
                queue.push_back(path);
            } else if filter.is_some_and(|f| !passes(f, &name)) {
                continue;
            } else if name.to_lowercase().contains(&q) {
                found.push(path);
            } else if reads < CONTENT_READS && std::fs::metadata(&path).is_ok_and(|m| m.len() <= CONTENT_MAX) && crate::thumbnails::is_text(&crate::thumbnails::mime_of(&path)) {
                reads += 1;
                if std::fs::read(&path).is_ok_and(|b| String::from_utf8_lossy(&b).to_lowercase().contains(&q)) {
                    by_content.push(path);
                }
            }
        }
    }
    found.extend(by_content);
    found.truncate(SEARCH_SHOWN);
    found
}

/// Search results from the file index, ordered as `search` orders its
/// own: files that are still there, not in hidden folders below `dir`,
/// passing `filter`; those whose names hold `query` first, nearest
/// first, then by name.
pub fn rank(dir: &Path, query: &str, found: Vec<PathBuf>, filter: Option<&Filter>) -> Vec<PathBuf> {
    let q = query.trim().to_lowercase();
    let mut kept: Vec<(bool, usize, String, PathBuf)> = found
        .into_iter()
        .filter_map(|path| {
            let below = path.strip_prefix(dir).ok()?;
            if below.iter().any(|part| part.to_string_lossy().starts_with('.')) || !path.is_file() {
                return None;
            }
            let name = path.file_name()?.to_string_lossy().into_owned();
            if filter.is_some_and(|f| !passes(f, &name)) {
                return None;
            }
            let lower = name.to_lowercase();
            Some((!lower.contains(&q), below.components().count(), lower, path))
        })
        .collect();
    kept.sort();
    kept.dedup_by(|a, b| a.3 == b.3);
    kept.into_iter().map(|k| k.3).take(SEARCH_SHOWN).collect()
}

/// Search below `dir`: the file index when one runs and finds something,
/// else `search` (on a blocking thread).
pub async fn find(conn: &zbus::Connection, dir: &Path, query: &str, filter: Option<&Filter>) -> Vec<PathBuf> {
    if let Some(found) = crate::file_index::search(conn, dir, query).await {
        let (dir2, query2, filter2) = (dir.to_path_buf(), query.to_owned(), filter.cloned());
        let found = tokio::task::spawn_blocking(move || rank(&dir2, &query2, found, filter2.as_ref())).await.unwrap_or_default();
        if !found.is_empty() {
            return found;
        }
    }
    let (dir, query, filter) = (dir.to_path_buf(), query.to_owned(), filter.cloned());
    tokio::task::spawn_blocking(move || search(&dir, &query, filter.as_ref())).await.unwrap_or_default()
}

// ----- Small helpers ----------------------------------------------------

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

/// A folder as the user reads it: `~` for the home folder.
pub fn shown_path(dir: &Path, home: &Path) -> String {
    match dir.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => dir.display().to_string(),
    }
}

/// A symbolic icon name for an entry: `folder-symbolic`, else the
/// content type's generic icon (`text-x-generic-symbolic`…).
pub fn icon_name(entry: &Entry) -> String {
    if entry.dir {
        return "folder-symbolic".into();
    }
    let generic = gio::content_type_get_generic_icon_name(&entry.content_type).map(|g| g.to_string()).unwrap_or_default();
    let known = ["text-x-generic", "image-x-generic", "audio-x-generic", "video-x-generic", "package-x-generic", "x-office-document", "x-office-spreadsheet", "x-office-presentation", "application-x-executable"];
    if known.contains(&generic.as_str()) {
        return format!("{generic}-symbolic");
    }
    "text-x-generic-symbolic".into()
}

/// Whether an entry is of a type that can be previewed (a picture,
/// text, or a type a thumbnailer handles), from its name alone.
pub fn previewable(entry: &Entry) -> bool {
    if entry.dir {
        return false;
    }
    let mime = gio::content_type_get_mime_type(&entry.content_type).map(|m| m.to_string()).unwrap_or_default();
    crate::thumbnails::is_picture(&mime) || crate::thumbnails::is_text(&mime) || crate::thumbnails::has_thumbnailer(&mime)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("helix-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn natural_order() {
        let mut names = vec!["slides-10.odp", "slides-2.odp", "slides-1.odp", "Slides-11.odp", "file10", "file2", "File1", "écrit", "eagle", "zebra", "Ångström", "apple", "img007", "img7", "img08"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, ["Ångström", "apple", "eagle", "écrit", "File1", "file2", "file10", "img7", "img007", "img08", "slides-1.odp", "slides-2.odp", "slides-10.odp", "Slides-11.odp", "zebra"]);
        assert_eq!(natural_cmp("file2", "file10"), Ordering::Less);
        assert_eq!(natural_cmp("Report", "report"), "Report".cmp("report"), "case ties keep a stable order");
        assert_eq!(natural_cmp("résumé", "resume-2"), Ordering::Less, "accents fold");
        assert_eq!(fold("Ínvoices"), "invoices");
    }

    fn entry(name: &str, dir: bool, size: u64, modified: i64) -> Entry {
        Entry { name: name.into(), path: PathBuf::from("/x").join(name), dir, size, modified, content_type: String::new(), origin: None }
    }

    #[test]
    fn sorting() {
        let entries = vec![entry("b10.txt", false, 30, 5), entry("Docs", true, 0, 9), entry("b2.txt", false, 300, 1), entry("apps", true, 0, 2), entry("a.png", false, 10, 7)];
        let order = |sort: Sort| {
            let mut rows: Vec<usize> = (0..entries.len()).collect();
            sort_rows(&entries, &mut rows, sort);
            rows.iter().map(|i| entries[*i].name.as_str()).collect::<Vec<_>>()
        };
        assert_eq!(order(Sort::default()), ["apps", "Docs", "a.png", "b2.txt", "b10.txt"]);
        assert_eq!(order(Sort { ascending: false, ..Sort::default() }), ["Docs", "apps", "b10.txt", "b2.txt", "a.png"], "folders stay first");
        assert_eq!(order(Sort { column: SortColumn::Size, ..Sort::default() }), ["apps", "Docs", "a.png", "b10.txt", "b2.txt"]);
        assert_eq!(order(Sort { column: SortColumn::Modified, ascending: false, folders_first: true }), ["Docs", "apps", "a.png", "b10.txt", "b2.txt"]);
        assert_eq!(order(Sort { column: SortColumn::Modified, ascending: true, folders_first: false }), ["b2.txt", "apps", "b10.txt", "a.png", "Docs"]);
        assert_eq!(SortColumn::from_key(SortColumn::Modified.key()), SortColumn::Modified);
        assert_eq!(SortColumn::from_key(SortColumn::Type.key()), SortColumn::Type);
        // By type as shown; ties by name.
        let types: Vec<String> = ["Text", "Folder", "Text", "Folder", "Image"].map(String::from).to_vec();
        let mut rows: Vec<usize> = (0..entries.len()).collect();
        sort_rows_typed(&entries, &mut rows, Sort { column: SortColumn::Type, ascending: true, folders_first: false }, &types);
        assert_eq!(rows.iter().map(|i| entries[*i].name.as_str()).collect::<Vec<_>>(), ["apps", "Docs", "a.png", "b2.txt", "b10.txt"]);
    }

    #[test]
    fn listing_reads_no_contents() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("list");
        std::fs::create_dir(dir.join("Sub")).unwrap();
        std::fs::write(dir.join("photo.png"), b"not really a png").unwrap();
        std::fs::write(dir.join("secret.pdf"), vec![1u8; 2048]).unwrap();
        std::fs::set_permissions(dir.join("secret.pdf"), std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::write(dir.join(".hidden"), b"").unwrap();
        let mut found = list(&dir).unwrap();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<&str> = found.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, [".hidden", "Sub", "photo.png", "secret.pdf"]);
        let secret = &found[3];
        assert_eq!((secret.dir, secret.size), (false, 2048), "an unreadable file is listed with its size");
        assert!(gio::content_type_is_mime_type(&secret.content_type, "application/pdf"), "type from the name: {}", secret.content_type);
        assert!(found[1].dir && found[0].hidden());
        assert_eq!(icon_name(&found[1]), "folder-symbolic");
        assert_eq!(icon_name(&found[2]), "image-x-generic-symbolic");
        assert!(previewable(&found[2]) && !previewable(&found[1]));
        std::fs::set_permissions(dir.join("secret.pdf"), std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(list(&dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn places_list() {
        let tmp = tmp("places");
        let (docs, mark, drive) = (tmp.join("Documents"), tmp.join("Projects"), tmp.join("usb"));
        for d in [&docs, &mark, &drive] {
            std::fs::create_dir_all(d).unwrap();
        }
        let marks = bookmarks(&format!("file://{} Work\nsftp://host/x Remote\nfile://{}\n", mark.display(), docs.display()));
        assert_eq!(marks, [("Work".to_owned(), mark.clone()), ("Documents".to_owned(), docs.clone())]);
        let trash = tmp.join("data/Trash/files");
        let list = places(&tmp, &[docs.clone(), tmp.join("Missing")], &marks, &[("USB".into(), drive.clone())], &[], true, true, Some(&trash));
        let kinds: Vec<(PlaceKind, &str)> = list.iter().map(|p| (p.kind, p.label.as_str())).collect();
        assert_eq!(kinds, [(PlaceKind::Recent, ""), (PlaceKind::Starred, ""), (PlaceKind::Home, ""), (PlaceKind::Standard, "Documents"), (PlaceKind::Trash, ""), (PlaceKind::Bookmark, "Work"), (PlaceKind::Drive, "USB"), (PlaceKind::Computer, ""), (PlaceKind::Other, "")]);
        assert_eq!(list[4].path.as_deref(), Some(trash.as_path()), "the Trash although its folder is not there yet");
        let saving = places(&tmp, &[], &[], &[], &[], false, false, None);
        assert!(!saving.iter().any(|p| matches!(p.kind, PlaceKind::Recent | PlaceKind::Starred | PlaceKind::Trash)), "no Recent, Starred or Trash: {saving:?}");
        assert_eq!(saving.last().map(|p| (p.kind, p.path.clone())), Some((PlaceKind::Other, None)), "Other Locations last");
        assert_eq!(gvfs_label("smb-share:server=nas,share=docs"), "docs on nas");
        assert_eq!(gvfs_label("sftp:host=box,user=me"), "me on box");
        assert_eq!(gvfs_label("dav:host=cloud,ssl=true"), "cloud");
        assert_eq!(gvfs_label("plain"), "plain");
        // Search: below the folder, hidden folders skipped, filtered.
        std::fs::create_dir_all(docs.join("2026/.cache")).unwrap();
        std::fs::write(docs.join("Report.pdf"), "").unwrap();
        std::fs::write(docs.join("2026/report-final.txt"), "").unwrap();
        std::fs::write(docs.join("2026/.cache/report.tmp"), "").unwrap();
        let found: Vec<String> = search(&tmp, "REPORT", None).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(found, ["Report.pdf", "report-final.txt"]);
        let pdf = ("PDF".to_owned(), vec![(0u32, "*.pdf".to_owned())]);
        assert_eq!(search(&tmp, "report", Some(&pdf)).len(), 1);
        assert!(search(&tmp, "  ", None).is_empty());
        // Text files holding the words come after names that do.
        std::fs::write(docs.join("minutes.txt"), "Quarterly REPORT due Friday").unwrap();
        std::fs::write(docs.join("photo.png"), "report").unwrap();
        let found: Vec<String> = search(&tmp, "report", None).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(found, ["Report.pdf", "report-final.txt", "minutes.txt"]);
        // The index's answers, ordered alike; gone, hidden and filtered
        // files left out.
        let index = vec![docs.join("minutes.txt"), docs.join("2026/report-final.txt"), docs.join("gone-report.txt"), docs.join("2026/.cache/report.tmp"), docs.join("Report.pdf"), docs.join("Report.pdf")];
        let names = |v: Vec<PathBuf>| v.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert_eq!(names(rank(&tmp, "report", index.clone(), None)), ["Report.pdf", "report-final.txt", "minutes.txt"]);
        assert_eq!(names(rank(&tmp, "report", index, Some(&pdf))), ["Report.pdf"]);
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
    fn merging() {
        // Rows merged batch by batch end as one sort of them all.
        let entries: Vec<Entry> = ["b10", "Docs", "b2", "apps", "a", "zeta", "Alpha", "b1"].iter().enumerate().map(|(i, n)| entry(n, i % 3 == 1, i as u64, 0)).collect();
        let sort = Sort::default();
        let mut keys = SortKeys::default();
        let mut rows = Vec::new();
        for range in [0..3, 3..4, 4..8] {
            keys.extend(&entries[..range.end], sort, &[]);
            let mut new: Vec<usize> = range.collect();
            new.sort_by(|&a, &b| keys.order(&entries, sort, a, b));
            merge_rows(&mut rows, new, |a, b| keys.order(&entries, sort, a, b));
        }
        let mut whole: Vec<usize> = (0..entries.len()).collect();
        sort_rows(&entries, &mut whole, sort);
        assert_eq!(rows, whole);
    }

    #[test]
    fn trash() {
        let root = tmp("trash");
        let trash = root.join("Trash");
        std::fs::create_dir_all(trash.join("info")).unwrap();
        std::fs::create_dir_all(trash.join("files/Old project")).unwrap();
        std::fs::write(trash.join("files/report.2.pdf"), "x").unwrap();
        std::fs::write(trash.join("info/report.2.pdf.trashinfo"), "[Trash Info]\nPath=/home/ada/Documents/Q3%20report.pdf\nDeletionDate=2026-11-01T10:00:00\n").unwrap();
        std::fs::write(trash.join("info/Old project.trashinfo"), "[Trash Info]\nPath=/home/ada/Old%20project\nDeletionDate=2026-11-02T10:00:00\n").unwrap();
        // An info without its item, and an item without info: not listed.
        std::fs::write(trash.join("info/gone.txt.trashinfo"), "[Trash Info]\nPath=/home/ada/gone.txt\n").unwrap();
        std::fs::write(trash.join("files/stray.txt"), "x").unwrap();
        let mut found = trashed(&trash, None);
        found.sort_by(|a, b| a.name.cmp(&b.name));
        let seen: Vec<(&str, bool, Option<&Path>, &Path)> = found.iter().map(|e| (e.name.as_str(), e.dir, e.origin.as_deref(), e.path.as_path())).collect();
        assert_eq!(seen, [("Old project", true, Some(Path::new("/home/ada")), trash.join("files/Old project").as_path()), ("Q3 report.pdf", false, Some(Path::new("/home/ada/Documents")), trash.join("files/report.2.pdf").as_path())]);
        assert!(gio::content_type_is_mime_type(&found[1].content_type, "application/pdf"), "the type from the name it had");
        // A drive's Trash: paths relative to the drive.
        let top = root.join("usb");
        let own = top.join(".Trash-1000");
        std::fs::create_dir_all(own.join("info")).unwrap();
        std::fs::create_dir_all(own.join("files")).unwrap();
        std::fs::write(own.join("files/a.txt"), "x").unwrap();
        std::fs::write(own.join("info/a.txt.trashinfo"), "[Trash Info]\nPath=Photos/a.txt\n").unwrap();
        assert_eq!(drive_trashes(&top, 1000), std::slice::from_ref(&own));
        assert!(drive_trashes(&top, 1001).is_empty());
        // A shared `.Trash` counts only when sticky.
        std::fs::create_dir_all(top.join(".Trash/1000")).unwrap();
        assert_eq!(drive_trashes(&top, 1000), std::slice::from_ref(&own), "not sticky");
        std::fs::set_permissions(top.join(".Trash"), std::os::unix::fs::PermissionsExt::from_mode(0o1777)).unwrap();
        assert_eq!(drive_trashes(&top, 1000), [top.join(".Trash/1000"), own.clone()]);
        let on_drive = trashed(&own, Some(&top));
        assert_eq!((on_drive[0].name.as_str(), on_drive[0].origin.clone()), ("a.txt", Some(top.join("Photos"))));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recent_privacy() {
        assert_eq!(recent_cutoff(false, -1, 1000), None);
        assert_eq!(recent_cutoff(true, 0, 1000), None);
        assert_eq!(recent_cutoff(true, -1, 1000), Some(i64::MIN));
        assert_eq!(recent_cutoff(true, 2, 200_000), Some(200_000 - 172_800));
    }

    #[test]
    fn globs_and_filters() {
        assert!(glob("*.png", "Photo.PNG"));
        assert!(glob("img-??.jpg", "img-01.jpg"));
        assert!(!glob("*.png", "notes.txt"));
        // Case-insensitive suffixes as Chromium and GTK send them.
        assert!(glob("*.[pP][dD][fF]", "Report.pdf"));
        assert!(glob("*.[pP][dD][fF]", "SCAN.PDF"));
        assert!(!glob("*.[pP][dD][fF]", "notes.txt"));
        assert!(!glob("*.[pP][dD][fF]", "a.pd"));
        assert!(glob("img[0-9].png", "img7.png"));
        assert!(!glob("img[0-9].png", "imgx.png"));
        assert!(glob("[!.]*", "visible"));
        assert!(!glob("[!.]*", ".hidden"));
        assert!(glob("[^.]*", "visible"));
        assert!(glob("[]x]", "]"));
        assert!(glob("a[b", "a[b"));
        assert!(!glob("a[b", "ab"));
        let upload: Filter = ("Custom Files".into(), vec![(0, "*.[jJ][pP][gG]".into()), (0, "*.[pP][nN][gG]".into())]);
        assert!(passes(&upload, "Holiday.JPG"));
        assert!(passes(&upload, "icon.png"));
        assert!(!passes(&upload, "cv.docx"));
        let any_image: Filter = ("Images".into(), vec![(1, "image/*".into())]);
        assert!(passes(&any_image, "a.png"));
        assert!(!passes(&any_image, "a.pdf"));
        let images: Filter = ("Images".into(), vec![(0, "*.jpg".into()), (1, "image/png".into())]);
        assert!(passes(&images, "a.jpg"));
        assert!(passes(&images, "b.png"));
        assert!(!passes(&images, "c.txt"));
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
    }
}
