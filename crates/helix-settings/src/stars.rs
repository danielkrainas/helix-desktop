//! Starred files, kept where GNOME Files (Nautilus) keeps them, so a
//! file starred in either is starred in both.
//!
//! The store is Nautilus's private TinySPARQL (Tracker 3) database,
//! `$XDG_DATA_HOME/nautilus/tags` (`meta.db`). GNOME publishes no
//! contract for it; this follows Nautilus 50's tag manager
//! (`nautilus-tag-manager.c`) and ontology (`nautilus.ontology`):
//! a starred file is `<uri> a nautilus:File ; nautilus:starred true`,
//! `uri` being its GIO URI, and only files inside the home folder are
//! starred. helixdesktop/spec, `contracts/files/starred.md`, has the
//! details.
//!
//! The same file is in helixdesktop/desktop
//! (`crates/helix-settings/src/stars.rs`) and helixdesktop/apps
//! (`apps/files/src/stars.rs`); keep the two identical. It needs only the
//! standard library: TinySPARQL, GLib and SQLite are loaded when a store
//! is opened, so a system without them has no stars rather than an app
//! that does not start.
//!
//! Nautilus's database is never migrated or changed in shape:
//! - It is opened read-only first, with the schema it holds (no
//!   ontology given, so TinySPARQL changes nothing), and the schema is
//!   checked: `nautilus:File` a class, `nautilus:starred` a property of
//!   it, a boolean, at most one. Anything else turns stars off.
//! - It is written only when the schema's version
//!   (`nrl:lastModified`) is the one known here and opening it for
//!   writing makes TinySPARQL rebuild nothing (the database's collation
//!   locale is this process's, its full-text flags Nautilus's); else
//!   stars are read-only.
//! - A missing store is created, as Nautilus creates it, only when
//!   something is starred: with Nautilus's own ontology when it is
//!   installed (and the known version), else with the same schema
//!   described here.
//!
//! Every call blocks (TinySPARQL waits up to 100 s for a lock): use it
//! off the UI thread.

#![allow(dead_code)]

use std::ffi::{c_char, c_int, c_long, c_void, CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// Nautilus's ontology namespace.
pub const NAMESPACE: &str = "https://gitlab.gnome.org/GNOME/nautilus#";
/// The schema version (`nrl:lastModified`) written for: Nautilus 3.38
/// to 50.
pub const ONTOLOGY_VERSION: &str = "2020-05-01T10:00:00Z";

const PREFIX: &str = "PREFIX nautilus: <https://gitlab.gnome.org/GNOME/nautilus#> ";

/// Nautilus's query for its starred files.
const STARRED: &str = "SELECT ?file { ?file a nautilus:File ; nautilus:starred true . }";

/// The schema's shape and version as the database holds it.
const SCHEMA: &str = "SELECT ?domain ?range ?card ?version { \
    nautilus:File a rdfs:Class . \
    nautilus:starred a rdf:Property ; rdfs:domain ?domain ; rdfs:range ?range . \
    OPTIONAL { nautilus:starred nrl:maxCardinality ?card } \
    OPTIONAL { nautilus: nrl:lastModified ?version } }";

/// Nautilus's schema, described here for creating the store when
/// Nautilus's own copy is not installed: the same namespace, prefix,
/// version, class and property.
const ONTOLOGY: &str = r#"@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
@prefix nrl: <http://tracker.api.gnome.org/ontology/v3/nrl#> .
@prefix nautilus: <https://gitlab.gnome.org/GNOME/nautilus#> .

nautilus: a nrl:Namespace, nrl:Ontology ;
    nrl:prefix "nautilus" ;
    nrl:lastModified "2020-05-01T10:00:00Z" .

nautilus:File a rdfs:Class ;
    rdfs:comment "A file, named by its URI." ;
    rdfs:subClassOf rdfs:Resource ;
    nrl:notify true .

nautilus:starred a rdf:Property ;
    rdfs:comment "Whether the file is starred." ;
    rdfs:domain nautilus:File ;
    rdfs:range xsd:boolean ;
    nrl:maxCardinality 1 .
"#;

/// What may be done with the stars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Read and starred or unstarred.
    ReadWrite,
    /// Read only (see [`Store::note`] for why).
    ReadOnly,
    /// No stars (see [`Store::note`] for why).
    Off,
}

// ----- Loading the libraries ----------------------------------------------

type Ptr = *mut c_void;

#[repr(C)]
struct GError {
    domain: u32,
    code: c_int,
    message: *const c_char,
}

extern "C" {
    fn dlopen(name: *const c_char, flags: c_int) -> Ptr;
    fn dlsym(handle: Ptr, name: *const c_char) -> Ptr;
    fn setlocale(category: c_int, locale: *const c_char) -> *mut c_char;
}

const RTLD_NOW: c_int = 2;
const LC_COLLATE: c_int = 3;

fn library(names: &[&str]) -> Option<Ptr> {
    names.iter().find_map(|n| {
        let n = CString::new(*n).ok()?;
        // SAFETY: a plain dlopen of a system library.
        let h = unsafe { dlopen(n.as_ptr(), RTLD_NOW) };
        (!h.is_null()).then_some(h)
    })
}

/// Symbol `name` of `lib` as a function pointer of type `T`.
///
/// # Safety
/// `T` must be the symbol's C type, as an `unsafe extern "C" fn`.
unsafe fn symbol<T: Copy>(lib: Ptr, name: &str) -> Option<T> {
    assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<Ptr>());
    let name = CString::new(name).ok()?;
    let p = dlsym(lib, name.as_ptr());
    (!p.is_null()).then(|| std::mem::transmute_copy::<Ptr, T>(&p))
}

struct Glib {
    free: unsafe extern "C" fn(Ptr),
    error_free: unsafe extern "C" fn(*mut GError),
    object_unref: unsafe extern "C" fn(Ptr),
    file_new_for_path: unsafe extern "C" fn(*const c_char) -> Ptr,
    filename_to_uri:
        unsafe extern "C" fn(*const c_char, *const c_char, *mut *mut GError) -> *mut c_char,
    filename_from_uri:
        unsafe extern "C" fn(*const c_char, *mut *mut c_char, *mut *mut GError) -> *mut c_char,
}

fn glib() -> Option<&'static Glib> {
    static LIB: OnceLock<Option<Glib>> = OnceLock::new();
    LIB.get_or_init(|| {
        let l = library(&["libgio-2.0.so.0"])?;
        // SAFETY: the types are GLib's.
        unsafe {
            Some(Glib {
                free: symbol(l, "g_free")?,
                error_free: symbol(l, "g_error_free")?,
                object_unref: symbol(l, "g_object_unref")?,
                file_new_for_path: symbol(l, "g_file_new_for_path")?,
                filename_to_uri: symbol(l, "g_filename_to_uri")?,
                filename_from_uri: symbol(l, "g_filename_from_uri")?,
            })
        }
    })
    .as_ref()
}

struct Tracker {
    glib: &'static Glib,
    connection_new: unsafe extern "C" fn(c_int, Ptr, Ptr, Ptr, *mut *mut GError) -> Ptr,
    query: unsafe extern "C" fn(Ptr, *const c_char, Ptr, *mut *mut GError) -> Ptr,
    update: unsafe extern "C" fn(Ptr, *const c_char, Ptr, *mut *mut GError),
    close: unsafe extern "C" fn(Ptr),
    next: unsafe extern "C" fn(Ptr, Ptr, *mut *mut GError) -> c_int,
    columns: unsafe extern "C" fn(Ptr) -> c_int,
    string: unsafe extern "C" fn(Ptr, c_int, *mut c_long) -> *const c_char,
}

/// TinySPARQL (`libtinysparql-3.0`, called `libtracker-sparql-3.0`
/// before 3.8; the same calls).
fn tracker() -> Option<&'static Tracker> {
    static LIB: OnceLock<Option<Tracker>> = OnceLock::new();
    LIB.get_or_init(|| {
        let glib = glib()?;
        let l = library(&["libtinysparql-3.0.so.0", "libtracker-sparql-3.0.so.0"])?;
        // SAFETY: the types are TinySPARQL's.
        unsafe {
            Some(Tracker {
                glib,
                connection_new: symbol(l, "tracker_sparql_connection_new")?,
                query: symbol(l, "tracker_sparql_connection_query")?,
                update: symbol(l, "tracker_sparql_connection_update")?,
                close: symbol(l, "tracker_sparql_connection_close")?,
                next: symbol(l, "tracker_sparql_cursor_next")?,
                columns: symbol(l, "tracker_sparql_cursor_get_n_columns")?,
                string: symbol(l, "tracker_sparql_cursor_get_string")?,
            })
        }
    })
    .as_ref()
}

struct Sqlite {
    open: unsafe extern "C" fn(*const c_char, *mut Ptr, c_int, *const c_char) -> c_int,
    prepare: unsafe extern "C" fn(Ptr, *const c_char, c_int, *mut Ptr, *mut *const c_char) -> c_int,
    step: unsafe extern "C" fn(Ptr) -> c_int,
    text: unsafe extern "C" fn(Ptr, c_int) -> *const c_char,
    finalize: unsafe extern "C" fn(Ptr) -> c_int,
    close: unsafe extern "C" fn(Ptr) -> c_int,
}

fn sqlite() -> Option<&'static Sqlite> {
    static LIB: OnceLock<Option<Sqlite>> = OnceLock::new();
    LIB.get_or_init(|| {
        let l = library(&["libsqlite3.so.0"])?;
        // SAFETY: the types are SQLite's.
        unsafe {
            Some(Sqlite {
                open: symbol(l, "sqlite3_open_v2")?,
                prepare: symbol(l, "sqlite3_prepare_v2")?,
                step: symbol(l, "sqlite3_step")?,
                text: symbol(l, "sqlite3_column_text")?,
                finalize: symbol(l, "sqlite3_finalize")?,
                close: symbol(l, "sqlite3_close")?,
            })
        }
    })
    .as_ref()
}

/// The message of `error` (which is freed), or `fallback`.
fn message(glib: &Glib, error: *mut GError, fallback: &str) -> String {
    if error.is_null() {
        return fallback.to_owned();
    }
    // SAFETY: a GError set by the call that failed.
    unsafe {
        let text = if (*error).message.is_null() {
            fallback.to_owned()
        } else {
            CStr::from_ptr((*error).message)
                .to_string_lossy()
                .into_owned()
        };
        (glib.error_free)(error);
        text
    }
}

fn c_path(path: &Path) -> Option<CString> {
    CString::new(path.as_os_str().as_bytes()).ok()
}

// ----- Paths and URIs -------------------------------------------------------

/// `$XDG_DATA_HOME`, else `~/.local/share` (as `g_get_user_data_dir`).
pub fn data_home() -> PathBuf {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
            .join(".local/share"),
    }
}

/// Nautilus's store under `data_home`.
pub fn store_dir(data_home: &Path) -> PathBuf {
    data_home.join("nautilus").join("tags")
}

/// Whether the store exists.
pub fn exists(data_home: &Path) -> bool {
    store_dir(data_home).join("meta.db").is_file()
}

/// The URI Nautilus stars `path` under (`g_filename_to_uri`, as
/// `g_file_get_uri` of a local file).
pub fn uri_for_path(path: &Path) -> Option<String> {
    let glib = glib()?;
    let p = c_path(path)?;
    let mut error = std::ptr::null_mut();
    // SAFETY: a valid C string; the result is GLib's to free.
    unsafe {
        let uri = (glib.filename_to_uri)(p.as_ptr(), std::ptr::null(), &mut error);
        if uri.is_null() {
            message(glib, error, "");
            return None;
        }
        let out = CStr::from_ptr(uri).to_str().ok().map(str::to_owned);
        (glib.free)(uri.cast());
        out
    }
}

/// The local path of a `file:` URI.
pub fn path_for_uri(uri: &str) -> Option<PathBuf> {
    let glib = glib()?;
    let u = CString::new(uri).ok()?;
    let mut error = std::ptr::null_mut();
    // SAFETY: a valid C string; the result is GLib's to free.
    unsafe {
        let p = (glib.filename_from_uri)(u.as_ptr(), std::ptr::null_mut(), &mut error);
        if p.is_null() {
            message(glib, error, "");
            return None;
        }
        let out = PathBuf::from(std::ffi::OsStr::from_bytes(CStr::from_ptr(p).to_bytes()));
        (glib.free)(p.cast());
        Some(out)
    }
}

/// Whether `path` may be starred: Nautilus stars only what is inside the
/// home folder (not the home folder itself).
pub fn can_star(home: &Path, path: &Path) -> bool {
    path != home && path.starts_with(home)
}

/// Whether `uri` can be written into a SPARQL `<…>` as it is (GIO's
/// file URIs always can).
fn plain_iri(uri: &str) -> bool {
    !uri.is_empty()
        && uri
            .chars()
            .all(|c| c > ' ' && !matches!(c, '<' | '>' | '"' | '{' | '}' | '|' | '^' | '`' | '\\'))
}

/// What changes when the store is written (by anyone): the database and
/// its write-ahead log's sizes and times. Compare two to know whether to
/// read the stars again.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stamp(Vec<Option<(u64, std::time::SystemTime)>>);

pub fn stamp(data_home: &Path) -> Stamp {
    let dir = store_dir(data_home);
    Stamp(
        ["meta.db", "meta.db-wal"]
            .iter()
            .map(|n| {
                let m = std::fs::metadata(dir.join(n)).ok()?;
                Some((m.len(), m.modified().ok()?))
            })
            .collect(),
    )
}

/// The installed Nautilus's ontology folder (Nautilus reads it from its
/// data folder, `/usr/share/nautilus/ontology`).
fn nautilus_ontology() -> Option<PathBuf> {
    let dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.split(':')
        .map(|d| Path::new(d).join("nautilus/ontology"))
        .find(|d| d.join("nautilus.ontology").is_file())
}

// ----- The store ----------------------------------------------------------

const READONLY: c_int = 1;

/// One TinySPARQL connection.
struct Conn {
    lib: &'static Tracker,
    ptr: Ptr,
}

// SAFETY: TinySPARQL connections may be used from any thread.
unsafe impl Send for Conn {}
unsafe impl Sync for Conn {}

impl Drop for Conn {
    fn drop(&mut self) {
        // SAFETY: our reference.
        unsafe {
            (self.lib.close)(self.ptr);
            (self.lib.glib.object_unref)(self.ptr);
        }
    }
}

/// Whether a failure is SQLite's "database is locked": another process
/// was writing at the same moment. Worth trying again.
fn locked(e: &str) -> bool {
    e.contains("database is locked") || e.contains("database table is locked")
}

impl Conn {
    /// Opens the store in `dir`; with `ontology`, TinySPARQL creates the
    /// store or brings its schema up to that ontology; without, it uses
    /// the schema the database holds.
    fn open(
        lib: &'static Tracker,
        dir: &Path,
        ontology: Option<&Path>,
        flags: c_int,
    ) -> Result<Conn, String> {
        let g = lib.glib;
        let d = c_path(dir).ok_or("bad path")?;
        let o = match ontology {
            Some(o) => Some(c_path(o).ok_or("bad path")?),
            None => None,
        };
        let mut error = std::ptr::null_mut();
        // SAFETY: GFiles made and unreferenced here; the connection is ours.
        unsafe {
            let store = (g.file_new_for_path)(d.as_ptr());
            let onto = o
                .as_ref()
                .map_or(std::ptr::null_mut(), |o| (g.file_new_for_path)(o.as_ptr()));
            let ptr = (lib.connection_new)(flags, store, onto, std::ptr::null_mut(), &mut error);
            (g.object_unref)(store);
            if !onto.is_null() {
                (g.object_unref)(onto);
            }
            if ptr.is_null() {
                return Err(message(g, error, "TinySPARQL could not open the store"));
            }
            Ok(Conn { lib, ptr })
        }
    }

    /// Runs `f` again while another process holds the lock (SQLite says
    /// "locked" rather than wait when two writers meet).
    fn retry<T>(mut f: impl FnMut() -> Result<T, String>) -> Result<T, String> {
        let mut tries = 0;
        loop {
            match f() {
                Err(e) if locked(&e) && tries < 20 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(10 * tries));
                }
                other => return other,
            }
        }
    }

    fn query(&self, sparql: &str) -> Result<Vec<Vec<String>>, String> {
        let q = CString::new(format!("{PREFIX}{sparql}")).map_err(|e| e.to_string())?;
        Self::retry(|| {
            let (lib, g) = (self.lib, self.lib.glib);
            let mut error = std::ptr::null_mut();
            // SAFETY: our connection; the cursor is unreferenced here.
            unsafe {
                let cursor = (lib.query)(self.ptr, q.as_ptr(), std::ptr::null_mut(), &mut error);
                if cursor.is_null() {
                    return Err(message(g, error, "the query failed"));
                }
                let mut rows = Vec::new();
                let result = loop {
                    let mut error = std::ptr::null_mut();
                    if (lib.next)(cursor, std::ptr::null_mut(), &mut error) == 0 {
                        break if error.is_null() {
                            Ok(())
                        } else {
                            Err(message(g, error, ""))
                        };
                    }
                    let n = (lib.columns)(cursor);
                    rows.push(
                        (0..n)
                            .map(|i| {
                                let s = (lib.string)(cursor, i, std::ptr::null_mut());
                                if s.is_null() {
                                    String::new()
                                } else {
                                    CStr::from_ptr(s).to_string_lossy().into_owned()
                                }
                            })
                            .collect(),
                    );
                };
                (g.object_unref)(cursor);
                result.map(|()| rows)
            }
        })
    }

    fn update(&self, sparql: &str) -> Result<(), String> {
        let q = CString::new(format!("{PREFIX}{sparql}")).map_err(|e| e.to_string())?;
        Self::retry(|| {
            let mut error = std::ptr::null_mut();
            // SAFETY: our connection.
            unsafe { (self.lib.update)(self.ptr, q.as_ptr(), std::ptr::null_mut(), &mut error) };
            if error.is_null() {
                Ok(())
            } else {
                Err(message(self.lib.glib, error, "the update failed"))
            }
        })
    }
}

/// Checks the schema the store holds: `Ok` with its version when stars
/// are what Nautilus 50 makes them, else why not.
fn check_schema(conn: &Conn) -> Result<String, String> {
    let rows = conn
        .query(SCHEMA)
        .map_err(|e| format!("the schema could not be read: {e}"))?;
    let [row] = rows.as_slice() else {
        return Err("the store has no nautilus:starred property of nautilus:File".into());
    };
    let (domain, range, card, version) = (&row[0], &row[1], &row[2], &row[3]);
    if domain.as_str() != format!("{NAMESPACE}File") {
        return Err(format!(
            "nautilus:starred applies to {domain}, not nautilus:File"
        ));
    }
    if range != "http://www.w3.org/2001/XMLSchema#boolean" {
        return Err(format!("nautilus:starred holds {range}, not a boolean"));
    }
    if card != "1" {
        return Err(format!(
            "nautilus:starred may hold {card:?} values, not one"
        ));
    }
    Ok(version.clone())
}

/// Why opening the store for writing would make TinySPARQL rebuild
/// something (its indexes for another collation locale, its full-text
/// tables for other flags), if it would.
fn rebuilds(db: &Path) -> Option<String> {
    let Some(s) = sqlite() else {
        return Some("SQLite could not be loaded to check the store".into());
    };
    let Some(p) = c_path(db) else {
        return Some("bad path".into());
    };
    const SQLITE_OPEN_READONLY: c_int = 1;
    const SQLITE_ROW: c_int = 100;
    let mut found: Vec<(String, String)> = Vec::new();
    // SAFETY: a read-only SQLite connection, closed here.
    unsafe {
        let mut db = std::ptr::null_mut();
        if (s.open)(p.as_ptr(), &mut db, SQLITE_OPEN_READONLY, std::ptr::null()) != 0 {
            (s.close)(db);
            return Some("the store's database could not be read".into());
        }
        let sql = c"SELECT key, value FROM metadata WHERE key IN ('locale', 'fts-flags')";
        let mut stmt = std::ptr::null_mut();
        if (s.prepare)(db, sql.as_ptr(), -1, &mut stmt, std::ptr::null_mut()) == 0 {
            while (s.step)(stmt) == SQLITE_ROW {
                let col = |i| {
                    let t = (s.text)(stmt, i);
                    (!t.is_null()).then(|| CStr::from_ptr(t).to_string_lossy().into_owned())
                };
                if let (Some(k), Some(v)) = (col(0), col(1)) {
                    found.push((k, v));
                }
            }
        }
        (s.finalize)(stmt);
        (s.close)(db);
    }
    let get = |k: &str| {
        found
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    // SAFETY: reading the current locale.
    let ours = unsafe {
        let l = setlocale(LC_COLLATE, std::ptr::null());
        (!l.is_null()).then(|| CStr::from_ptr(l).to_string_lossy().into_owned())
    };
    if get("locale") != ours.as_deref() {
        return Some(format!(
            "the store sorts for locale {:?}, this app for {:?}",
            get("locale").unwrap_or("none"),
            ours.as_deref().unwrap_or("none")
        ));
    }
    // Nautilus opens it without full-text options: "0" (or not yet
    // written, which Nautilus's next start writes the same way).
    match get("fts-flags") {
        None | Some("0") => None,
        Some(f) => Some(format!("the store has full-text flags {f}, not Nautilus's")),
    }
}

/// Nautilus's star store.
pub struct Store {
    dir: PathBuf,
    conn: Option<Conn>,
    access: Access,
    note: String,
}

impl Store {
    fn new(dir: PathBuf, conn: Option<Conn>, access: Access, note: impl Into<String>) -> Store {
        Store {
            dir,
            conn,
            access,
            note: note.into(),
        }
    }

    /// Opens the store under `data_home` (see the module notes): for
    /// starring when `write`, else to read only. Never fails: a store
    /// that cannot be used is [`Access::Off`], with the reason in
    /// [`Store::note`]. A missing store has no stars; one that may be
    /// written is created on the first star.
    pub fn open(data_home: &Path, write: bool) -> Store {
        let dir = store_dir(data_home);
        let Some(lib) = tracker() else {
            return Store::new(dir, None, Access::Off, "TinySPARQL is not installed");
        };
        let db = dir.join("meta.db");
        if !db.is_file() {
            return if write {
                Store::new(dir, None, Access::ReadWrite, "no stars yet")
            } else {
                Store::new(dir, None, Access::ReadOnly, "no stars yet")
            };
        }
        // Read-only, with the schema the database holds: TinySPARQL
        // changes nothing (it refuses a database it would have to
        // update).
        let ro = match Conn::open(lib, &dir, None, READONLY) {
            Ok(c) => c,
            Err(e) => {
                return Store::new(
                    dir,
                    None,
                    Access::Off,
                    format!("the store cannot be read: {e}"),
                )
            }
        };
        let version = match check_schema(&ro) {
            Ok(v) => v,
            Err(e) => return Store::new(dir, None, Access::Off, e),
        };
        if !write {
            return Store::new(dir, Some(ro), Access::ReadOnly, "read-only");
        }
        if version != ONTOLOGY_VERSION {
            let note = format!("Nautilus's schema is version {version:?}, not {ONTOLOGY_VERSION}");
            return Store::new(dir, Some(ro), Access::ReadOnly, note);
        }
        if let Some(why) = rebuilds(&db) {
            return Store::new(dir, Some(ro), Access::ReadOnly, why);
        }
        drop(ro);
        // For writing, still with the schema the database holds.
        match Conn::open(lib, &dir, None, 0) {
            Ok(c) => Store::new(dir, Some(c), Access::ReadWrite, ""),
            Err(e) => match Conn::open(lib, &dir, None, READONLY) {
                Ok(ro) => Store::new(
                    dir,
                    Some(ro),
                    Access::ReadOnly,
                    format!("the store cannot be written: {e}"),
                ),
                Err(_) => Store::new(
                    dir,
                    None,
                    Access::Off,
                    format!("the store cannot be opened: {e}"),
                ),
            },
        }
    }

    pub fn access(&self) -> Access {
        self.access
    }

    /// Whether the store is open (else there are no stars yet, or none
    /// can be read).
    pub fn is_open(&self) -> bool {
        self.conn.is_some()
    }

    /// Why the stars are read-only or off (or "no stars yet").
    pub fn note(&self) -> &str {
        &self.note
    }

    /// The starred files' URIs (all of them; Nautilus shows those inside
    /// the home folder).
    pub fn starred(&self) -> Result<Vec<String>, String> {
        if self.access == Access::Off {
            return Err(self.note.clone());
        }
        let Some(conn) = &self.conn else {
            return Ok(Vec::new());
        };
        Ok(conn
            .query(STARRED)?
            .into_iter()
            .filter_map(|r| r.into_iter().next())
            .collect())
    }

    /// The connection to write with, creating the store if there is none
    /// (with Nautilus's ontology when installed, else the same schema
    /// described here; nothing when the installed Nautilus has another
    /// version).
    fn writer(&mut self) -> Result<&Conn, String> {
        if self.access != Access::ReadWrite {
            return Err(self.note.clone());
        }
        if self.conn.is_none() {
            let lib = tracker().ok_or("TinySPARQL is not installed")?;
            if self.dir.join("meta.db").exists() {
                // Made since (by Nautilus): open it as any other.
                let data_home = self.dir.parent().and_then(Path::parent).ok_or("bad path")?;
                *self = Store::open(data_home, true);
                return match (&self.conn, self.access) {
                    (Some(c), Access::ReadWrite) => Ok(c),
                    _ => Err(self.note.clone()),
                };
            }
            let conn = match nautilus_ontology() {
                Some(dir) => {
                    let text =
                        std::fs::read_to_string(dir.join("nautilus.ontology")).unwrap_or_default();
                    if !text.contains(&format!("nrl:lastModified \"{ONTOLOGY_VERSION}\"")) {
                        self.access = Access::ReadOnly;
                        self.note = "the installed Nautilus has another schema version".into();
                        return Err(self.note.clone());
                    }
                    Conn::open(lib, &self.dir, Some(&dir), 0)?
                }
                None => {
                    let tmp = std::env::temp_dir().join(format!(
                        "helix-stars-ontology-{}-{:?}",
                        std::process::id(),
                        std::thread::current().id()
                    ));
                    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
                    let made = std::fs::write(tmp.join("nautilus.ontology"), ONTOLOGY)
                        .map_err(|e| e.to_string())
                        .and_then(|()| Conn::open(lib, &self.dir, Some(&tmp), 0));
                    let _ = std::fs::remove_dir_all(&tmp);
                    made?
                }
            };
            check_schema(&conn)?;
            self.conn = Some(conn);
            self.note.clear();
        }
        self.conn.as_ref().ok_or_else(|| "no store".into())
    }

    /// Stars (`on`) or unstars `uris`, as Nautilus does.
    pub fn set_starred(&mut self, uris: &[String], on: bool) -> Result<(), String> {
        if uris.is_empty() {
            return Ok(());
        }
        if let Some(bad) = uris.iter().find(|u| !plain_iri(u)) {
            return Err(format!("not a plain URI: {bad}"));
        }
        let triples: String = uris
            .iter()
            .map(|u| format!("<{u}> a nautilus:File ; nautilus:starred true . "))
            .collect();
        let verb = if on { "INSERT" } else { "DELETE" };
        self.writer()?
            .update(&format!("{verb} DATA {{ {triples}}}"))
    }

    /// `from` was renamed or moved to `to`: its star and those of files
    /// inside it follow (Nautilus's `update_moved_uris`). Answers how many
    /// moved.
    pub fn moved(&mut self, from: &str, to: &str) -> Result<usize, String> {
        if self.conn.is_none() || from == to {
            return Ok(0);
        }
        let inside = format!("{}/", from.trim_end_matches('/'));
        let pairs: Vec<(String, String)> = self
            .starred()?
            .into_iter()
            .filter_map(|u| {
                if u == from {
                    Some((u, to.to_owned()))
                } else {
                    let rest = u.strip_prefix(&inside)?.to_owned();
                    Some((u, format!("{}/{rest}", to.trim_end_matches('/'))))
                }
            })
            .filter(|(a, b)| plain_iri(a) && plain_iri(b))
            .collect();
        if pairs.is_empty() {
            return Ok(0);
        }
        let triples = |i: usize| -> String {
            pairs
                .iter()
                .map(|p| {
                    let u = if i == 0 { &p.0 } else { &p.1 };
                    format!("<{u}> a nautilus:File ; nautilus:starred true . ")
                })
                .collect()
        };
        let sparql = format!(
            "DELETE DATA {{ {}}} ; INSERT DATA {{ {}}}",
            triples(0),
            triples(1)
        );
        self.writer()?.update(&sparql)?;
        Ok(pairs.len())
    }
}

/// The starred files inside `home` that exist, as paths, from a
/// read-only look at the store under `data_home`.
pub fn starred_paths(data_home: &Path, home: &Path) -> Result<Vec<PathBuf>, String> {
    let store = Store::open(data_home, false);
    let mut out: Vec<PathBuf> = store
        .starred()?
        .iter()
        .filter(|u| u.starts_with("file://"))
        .filter_map(|u| path_for_uri(u))
        .filter(|p| can_star(home, p) && std::fs::symlink_metadata(p).is_ok())
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("helix-stars-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn uris_as_gio_writes_them() {
        if glib().is_none() {
            return;
        }
        let p = Path::new("/home/ada/Q3 report #2 (é).pdf");
        let uri = uri_for_path(p).unwrap();
        assert_eq!(uri, "file:///home/ada/Q3%20report%20%232%20(%C3%A9).pdf");
        assert!(plain_iri(&uri));
        assert_eq!(path_for_uri(&uri).as_deref(), Some(p));
        assert!(can_star(Path::new("/home/ada"), p));
        assert!(!can_star(Path::new("/home/ada"), Path::new("/home/ada")));
        assert!(!can_star(Path::new("/home/ada"), Path::new("/home/adam/x")));
        assert!(!plain_iri("file:///a>b") && !plain_iri("file:///a b") && !plain_iri(""));
    }

    /// Created on the first star (not before), starred, unstarred, moved
    /// (with what is inside), read back read-only.
    #[test]
    fn star_unstar_move() {
        if tracker().is_none() {
            eprintln!("TinySPARQL is not installed; skipped");
            return;
        }
        let data = scratch("basic");
        let ro = Store::open(&data, false);
        assert_eq!((ro.access(), ro.starred()), (Access::ReadOnly, Ok(vec![])));
        let mut s = Store::open(&data, true);
        assert_eq!(s.access(), Access::ReadWrite);
        assert!(!exists(&data), "not made before a star");
        let a = "file:///home/ada/a.txt".to_owned();
        let dir = "file:///home/ada/Work".to_owned();
        let inner = "file:///home/ada/Work/b%20c.txt".to_owned();
        s.set_starred(&[a.clone(), dir.clone(), inner.clone()], true)
            .unwrap();
        assert!(exists(&data));
        let mut got = s.starred().unwrap();
        got.sort();
        let mut want = [a.clone(), dir.clone(), inner.clone()];
        want.sort();
        assert_eq!(got, want);
        s.set_starred(std::slice::from_ref(&a), false).unwrap();
        assert_eq!(s.moved(&dir, "file:///home/ada/Old%20Work"), Ok(2));
        assert_eq!(
            s.moved("file:///home/ada/Wor", "file:///x"),
            Ok(0),
            "a name sharing the start is not inside"
        );
        drop(s);
        let ro = Store::open(&data, false);
        let mut got = ro.starred().unwrap();
        got.sort();
        assert_eq!(
            got,
            [
                "file:///home/ada/Old%20Work",
                "file:///home/ada/Old%20Work/b%20c.txt"
            ]
        );
        assert!(
            Store::open(&data, false).set_starred(&[a], true).is_err(),
            "read-only"
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    /// A store made here is one Nautilus opens with its own ontology
    /// without changing anything but its comments, and the stars stay.
    #[test]
    fn nautilus_opens_ours() {
        let (Some(lib), Some(onto)) = (tracker(), nautilus_ontology()) else {
            eprintln!("TinySPARQL or Nautilus is not installed; skipped");
            return;
        };
        let data = scratch("nautilus");
        let mut s = Store::open(&data, true);
        s.set_starred(&["file:///home/ada/a".to_owned()], true)
            .unwrap();
        drop(s);
        // As Nautilus opens it.
        let n = Conn::open(lib, &store_dir(&data), Some(&onto), 0).unwrap();
        assert_eq!(n.query(STARRED).unwrap(), [["file:///home/ada/a"]]);
        n.update("INSERT DATA { <file:///home/ada/b> a nautilus:File ; nautilus:starred true . }")
            .unwrap();
        drop(n);
        let s = Store::open(&data, true);
        assert_eq!(s.access(), Access::ReadWrite, "{}", s.note());
        assert_eq!(s.starred().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&data);
    }

    fn store_with(data: &Path, ontology: &str) {
        let lib = tracker().unwrap();
        let onto = data.join("onto");
        std::fs::create_dir_all(&onto).unwrap();
        std::fs::write(onto.join("x.ontology"), ontology).unwrap();
        drop(Conn::open(lib, &store_dir(data), Some(&onto), 0).unwrap());
    }

    /// A schema not known: another version is read-only, another shape
    /// (or none) turns stars off; neither is changed.
    #[test]
    fn unknown_schemas() {
        if tracker().is_none() {
            return;
        }
        let data = scratch("newer");
        store_with(
            &data,
            &ONTOLOGY.replace(ONTOLOGY_VERSION, "2030-01-01T00:00:00Z"),
        );
        let before = std::fs::read(store_dir(&data).join("meta.db")).unwrap();
        let s = Store::open(&data, true);
        assert_eq!(s.access(), Access::ReadOnly);
        assert!(s.note().contains("2030"), "{}", s.note());
        assert_eq!(s.starred(), Ok(vec![]));
        drop(s);
        assert_eq!(
            std::fs::read(store_dir(&data).join("meta.db")).unwrap(),
            before
        );
        let _ = std::fs::remove_dir_all(&data);

        let data = scratch("string");
        store_with(&data, &ONTOLOGY.replace("xsd:boolean", "xsd:string"));
        let s = Store::open(&data, true);
        assert_eq!(s.access(), Access::Off);
        assert!(s.note().contains("not a boolean"), "{}", s.note());
        assert!(s.starred().is_err());
        let _ = std::fs::remove_dir_all(&data);

        let data = scratch("other");
        store_with(
            &data,
            &ONTOLOGY.replace("nautilus:starred", "nautilus:liked"),
        );
        let s = Store::open(&data, false);
        assert_eq!(s.access(), Access::Off);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// Changes show in the stamp.
    #[test]
    fn stamp_changes() {
        if tracker().is_none() {
            return;
        }
        let data = scratch("stamp");
        let empty = stamp(&data);
        let mut s = Store::open(&data, true);
        s.set_starred(&["file:///home/ada/a".to_owned()], true)
            .unwrap();
        let one = stamp(&data);
        assert_ne!(empty, one);
        std::thread::sleep(Duration::from_millis(20));
        s.set_starred(&["file:///home/ada/b".to_owned()], true)
            .unwrap();
        assert_ne!(stamp(&data), one);
        let _ = std::fs::remove_dir_all(&data);
    }
}
