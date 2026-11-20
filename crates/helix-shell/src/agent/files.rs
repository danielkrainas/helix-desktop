//! Files for agents at the *Read only* level (helixdesktop/spec
//! `contracts/agent-access/README.md`, "Files"; helixdesktop/desktop#146):
//! recently used files from `recently-used.xbel`, and a file search
//! through LocalSearch (Tracker) when it runs, else a bounded scan of the
//! folder (`helix_settings::files::find`, as the file chooser searches);
//! and (helixdesktop/desktop#156) the files starred in the store GNOME
//! Files and Helix Files share (`helix_settings::stars`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gio::prelude::*;

use super::desktop::{same_app, Installed, Record};
use super::policy::{self, Config};
use super::value;

/// The most files `RecentFiles` and `SearchFiles` return, and how many
/// without a `limit`.
pub const MAX_FILES: usize = 200;
pub const DEFAULT_FILES: usize = 50;

/// A recently used item as `recently-used.xbel` has it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Recent {
    pub uri: String,
    pub mime_type: String,
    /// When it was last used (added, modified or visited), Unix seconds.
    pub used: i64,
    /// The apps that registered it: (name, the first word of its command).
    pub apps: Vec<(String, String)>,
    /// Only the apps that registered it should show it.
    pub private: bool,
}

/// Every item of the bookmark file at `path` (none when it cannot be
/// read).
pub fn read_xbel(path: &Path) -> Vec<Recent> {
    std::fs::read_to_string(path).map(|text| parse_xbel(&text)).unwrap_or_default()
}

/// The items of an XBEL bookmark file (`recently-used.xbel`).
pub fn parse_xbel(text: &str) -> Vec<Recent> {
    let Ok(doc) = roxmltree::Document::parse(text) else { return Vec::new() };
    let when = |stamp: Option<&str>| stamp.and_then(|s| gio::glib::DateTime::from_iso8601(s, None).ok()).map_or(0, |d| d.to_unix());
    doc.root_element()
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "bookmark")
        .filter_map(|b| {
            let uri = b.attribute("href")?.to_owned();
            let mut used = ["added", "modified", "visited"].iter().map(|a| when(b.attribute(*a))).max().unwrap_or(0);
            let mut item = Recent { uri, ..Recent::default() };
            for n in b.descendants().filter(|n| n.is_element()) {
                match n.tag_name().name() {
                    "mime-type" => item.mime_type = n.attribute("type").unwrap_or("").to_owned(),
                    "private" => item.private = true,
                    "application" => {
                        let name = n.attribute("name").unwrap_or("").to_owned();
                        let exec = n.attribute("exec").unwrap_or("");
                        let program = exec.split_whitespace().next().unwrap_or("").trim_matches(['\'', '"']).rsplit('/').next().unwrap_or("").to_owned();
                        used = used.max(when(n.attribute("modified")));
                        item.apps.push((name, program));
                    }
                    _ => {}
                }
            }
            item.used = used;
            Some(item)
        })
        .collect()
}

/// `recently-used.xbel` in the data folder.
pub fn xbel_path() -> PathBuf {
    gio::glib::user_data_dir().join("recently-used.xbel")
}

/// Whether an app that registered a recent item under `name` (with
/// command `program`) is `app`.
pub fn registered_by(app: &Installed, name: &str, program: &str) -> bool {
    let exe = app.exec.as_deref().unwrap_or("");
    [name, program].iter().any(|n| !n.is_empty() && (same_app(n, &app.id) || app.wm_class.as_deref().is_some_and(|c| same_app(n, c)) || *n == app.name || *n == exe))
}

/// Whether app id `app_id` (a window's) registered `item`.
pub fn registered_by_window(installed: &[Installed], app_id: &str, item: &Recent) -> bool {
    item.apps.iter().any(|(name, program)| {
        same_app(name, app_id) || same_app(program, app_id) || installed.iter().filter(|a| same_app(&a.id, app_id) || a.wm_class.as_deref().is_some_and(|c| same_app(c, app_id))).any(|a| registered_by(a, name, program))
    })
}

/// Whether `item` is left out: private, or registered only by excluded
/// apps (Settings and the user's exclusions).
pub fn hidden(item: &Recent, installed: &[Installed], config: &Config) -> bool {
    if item.private {
        return true;
    }
    let excluded = |name: &str, program: &str| {
        policy::hidden(config, name)
            || policy::hidden(config, program)
            || installed.iter().filter(|a| policy::hidden(config, &a.id)).any(|a| registered_by(a, name, program))
    };
    !item.apps.is_empty() && item.apps.iter().all(|(name, program)| excluded(name, program))
}

/// Whether `mime_type` is `wanted` (a type, or `major/*`).
pub fn mime_matches(mime_type: &str, wanted: &str) -> bool {
    if wanted.is_empty() || mime_type == wanted {
        return true;
    }
    match wanted.strip_suffix("/*") {
        Some(major) => mime_type.split('/').next() == Some(major),
        None => gio::content_type_is_mime_type(mime_type, wanted),
    }
}

/// A File record's facts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileInfo {
    pub uri: String,
    pub name: String,
    pub mime_type: String,
    pub modified: i64,
    /// For local files.
    pub size: Option<u64>,
    /// When it was used (recent files).
    pub used: Option<i64>,
}

impl FileInfo {
    /// A local file's facts from its metadata (`None` when it is gone or
    /// is a folder).
    pub fn local(path: &Path) -> Option<FileInfo> {
        let entry = helix_settings::files::Entry::at(path.to_path_buf()).filter(|e| !e.dir)?;
        Some(FileInfo {
            uri: gio::File::for_path(path).uri().to_string(),
            name: entry.name,
            mime_type: entry.content_type,
            modified: entry.modified,
            size: Some(entry.size),
            used: None,
        })
    }

    pub fn record(&self) -> Record {
        let mut r = Record::from([
            ("uri".to_owned(), value(self.uri.as_str())),
            ("name".to_owned(), value(self.name.as_str())),
            ("mime-type".to_owned(), value(self.mime_type.as_str())),
            ("modified".to_owned(), value(self.modified)),
        ]);
        if let Some(size) = self.size {
            r.insert("size".to_owned(), value(size));
        }
        if let Some(used) = self.used {
            r.insert("used".to_owned(), value(used));
        }
        r
    }
}

/// The name to show for `uri`: its last part, unescaped.
pub fn display_name(uri: &str) -> String {
    let last = uri.trim_end_matches('/').rsplit('/').next().unwrap_or(uri);
    gio::glib::Uri::unescape_string(last, None::<&str>).map_or_else(|| last.to_owned(), |s| s.to_string())
}

/// Recently used files, newest first: local files that still exist (with
/// their metadata) and other locations, left out when private or used
/// only by excluded apps, used at or after `cutoff` (the privacy
/// settings' age), of type `mime` when given, at most `limit`.
pub fn recent(items: Vec<Recent>, installed: &[Installed], config: &Config, cutoff: i64, mime: Option<&str>, limit: usize) -> Vec<FileInfo> {
    let mut items: Vec<Recent> = items.into_iter().filter(|i| i.used >= cutoff && !hidden(i, installed, config)).collect();
    items.sort_by_key(|i| std::cmp::Reverse(i.used));
    let mut out = Vec::new();
    for item in items {
        if out.len() >= limit {
            break;
        }
        let info = if item.uri.starts_with("file://") {
            let Some(path) = gio::File::for_uri(&item.uri).path() else { continue };
            let Some(mut info) = FileInfo::local(&path) else { continue };
            if !item.mime_type.is_empty() {
                info.mime_type = item.mime_type.clone();
            }
            info
        } else {
            FileInfo { uri: item.uri.clone(), name: display_name(&item.uri), mime_type: item.mime_type.clone(), modified: item.used, size: None, used: None }
        };
        if mime.is_some_and(|m| !mime_matches(&info.mime_type, m)) {
            continue;
        }
        out.push(FileInfo { used: Some(item.used), ..info });
    }
    out
}

/// Starred files (README.md, "Media, power and stars"): those of the
/// shared store (`helix_settings::stars`, under `data_home`, opened
/// read-only as the file chooser opens it) inside `home` that still
/// exist, folders too, most recently modified first, of type `mime` when
/// given, at most `limit`. None when the store is missing, off or cannot
/// be read. It blocks (TinySPARQL may wait for a lock).
pub fn starred(home: &Path, data_home: &Path, mime: Option<&str>, limit: usize) -> Vec<FileInfo> {
    let paths = match helix_settings::stars::starred_paths(data_home, home) {
        Ok(paths) => paths,
        Err(err) => {
            tracing::debug!(%err, "no starred files for agents");
            return Vec::new();
        }
    };
    let mut out: Vec<FileInfo> = paths
        .into_iter()
        .filter_map(|path| {
            let entry = helix_settings::files::Entry::at(path.clone())?;
            Some(FileInfo {
                uri: gio::File::for_path(&path).uri().to_string(),
                name: entry.name,
                mime_type: entry.content_type,
                modified: entry.modified,
                size: (!entry.dir).then_some(entry.size),
                used: None,
            })
        })
        .filter(|f| mime.is_none_or(|m| mime_matches(&f.mime_type, m)))
        .collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.uri.cmp(&b.uri)));
    out.truncate(limit);
    out
}

/// What `SearchFiles` was asked.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Search {
    pub query: String,
    pub folder: PathBuf,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub mime: Option<String>,
    pub limit: usize,
}

/// `SearchFiles`'s options: `since`, `until` (`x`), `type`, `in` (`s`, a
/// folder's path or `file://` URI; default the home folder), `limit`
/// (`u`).
pub fn search_options(query: &str, options: &HashMap<String, zbus::zvariant::OwnedValue>) -> Result<Search, super::AgentError> {
    use super::AgentError;
    let bad = |key: &str| AgentError::invalid_args(format!("bad “{key}”"));
    let time = |key: &str| options.get(key).map(|v| i64::try_from(v.clone()).map_err(|_| bad(key))).transpose();
    let text = |key: &str| options.get(key).map(|v| String::try_from(v.clone()).map_err(|_| bad(key))).transpose();
    if query.trim().is_empty() {
        return Err(AgentError::invalid_args("the query is empty"));
    }
    let folder = match text("in")?.filter(|f| !f.is_empty()) {
        None => gio::glib::home_dir(),
        Some(f) if f.contains("://") => gio::File::for_uri(&f).path().ok_or_else(|| AgentError::invalid_args("“in” is not a local folder"))?,
        Some(f) => PathBuf::from(f),
    };
    if !folder.is_absolute() {
        return Err(AgentError::invalid_args("“in” must be an absolute path"));
    }
    if !folder.is_dir() {
        return Err(AgentError::NotFound(folder.display().to_string()));
    }
    Ok(Search { query: query.to_owned(), folder, since: time("since")?, until: time("until")?, mime: text("type")?.filter(|m| !m.is_empty()), limit: limit(options)? })
}

/// The `limit` option (`u`): 1–[`MAX_FILES`], default [`DEFAULT_FILES`].
pub fn limit(options: &HashMap<String, zbus::zvariant::OwnedValue>) -> Result<usize, super::AgentError> {
    match options.get("limit") {
        None => Ok(DEFAULT_FILES),
        Some(v) => u32::try_from(v.clone()).map(|n| (n as usize).clamp(1, MAX_FILES)).map_err(|_| super::AgentError::invalid_args("“limit” must be a number (u)")),
    }
}

/// Search: names and contents through LocalSearch when it runs and finds
/// something, else names (and small text files' contents) by scanning
/// below the folder; hidden files left out; newest first after the
/// filters.
pub async fn search(conn: &zbus::Connection, s: &Search) -> Vec<FileInfo> {
    let filter = s.mime.as_ref().map(|m| (String::new(), vec![(1u32, m.clone())]));
    let found = helix_settings::files::find(conn, &s.folder, &s.query, filter.as_ref()).await;
    let s = s.clone();
    tokio::task::spawn_blocking(move || {
        let mut out: Vec<FileInfo> = found
            .iter()
            .filter_map(|p| FileInfo::local(p))
            .filter(|f| s.since.is_none_or(|t| f.modified >= t) && s.until.is_none_or(|t| f.modified < t))
            .filter(|f| s.mime.as_deref().is_none_or(|m| mime_matches(&f.mime_type, m)))
            .collect();
        out.truncate(s.limit);
        out
    })
    .await
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const XBEL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xbel version="1.0" xmlns:bookmark="http://www.freedesktop.org/standards/desktop-bookmarks" xmlns:mime="http://www.freedesktop.org/standards/shared-mime-info">
  <bookmark href="file://DIR/notes%20one.txt" added="2026-11-18T10:00:00Z" modified="2026-11-18T10:00:00Z" visited="2026-11-19T09:00:00Z">
    <info><metadata owner="http://freedesktop.org"><mime:mime-type type="text/plain"/>
      <bookmark:applications><bookmark:application name="Text Editor" exec="&apos;gnome-text-editor %u&apos;" modified="2026-11-19T09:00:00Z" count="2"/></bookmark:applications>
    </metadata></info>
  </bookmark>
  <bookmark href="file://DIR/report.pdf" added="2026-11-17T10:00:00Z" modified="2026-11-17T10:00:00Z" visited="2026-11-17T10:00:00Z">
    <info><metadata owner="http://freedesktop.org"><mime:mime-type type="application/pdf"/>
      <bookmark:applications><bookmark:application name="org.gnome.Papers" exec="&apos;papers %u&apos;" modified="2026-11-17T10:00:00Z" count="1"/></bookmark:applications>
    </metadata></info>
  </bookmark>
  <bookmark href="file://DIR/vault.kdbx" added="2026-11-19T10:00:00Z" modified="2026-11-19T10:00:00Z" visited="2026-11-19T10:00:00Z">
    <info><metadata owner="http://freedesktop.org"><mime:mime-type type="application/x-keepass2"/>
      <bookmark:applications><bookmark:application name="keepassxc" exec="&apos;keepassxc %u&apos;" modified="2026-11-19T10:00:00Z" count="1"/></bookmark:applications>
    </metadata></info>
  </bookmark>
  <bookmark href="file://DIR/secret.txt" added="2026-11-19T11:00:00Z" modified="2026-11-19T11:00:00Z" visited="2026-11-19T11:00:00Z">
    <info><metadata owner="http://freedesktop.org"><mime:mime-type type="text/plain"/><bookmark:private/>
      <bookmark:applications><bookmark:application name="Text Editor" exec="&apos;gnome-text-editor %u&apos;" modified="2026-11-19T11:00:00Z" count="1"/></bookmark:applications>
    </metadata></info>
  </bookmark>
  <bookmark href="file://DIR/gone.txt" added="2026-11-19T12:00:00Z" modified="2026-11-19T12:00:00Z" visited="2026-11-19T12:00:00Z">
    <info><metadata owner="http://freedesktop.org"><mime:mime-type type="text/plain"/>
      <bookmark:applications><bookmark:application name="Text Editor" exec="&apos;gnome-text-editor %u&apos;" modified="2026-11-19T12:00:00Z" count="1"/></bookmark:applications>
    </metadata></info>
  </bookmark>
  <bookmark href="sftp://host/share/plan.odt" added="2026-11-16T12:00:00Z" modified="2026-11-16T12:00:00Z" visited="2026-11-16T12:00:00Z">
    <info><metadata owner="http://freedesktop.org"><mime:mime-type type="application/vnd.oasis.opendocument.text"/>
      <bookmark:applications><bookmark:application name="LibreOffice" exec="&apos;soffice %u&apos;" modified="2026-11-16T12:00:00Z" count="1"/></bookmark:applications>
    </metadata></info>
  </bookmark>
</xbel>
"#;

    fn installed() -> Vec<Installed> {
        vec![
            Installed { id: "org.gnome.TextEditor".into(), name: "Text Editor".into(), wm_class: None, exec: Some("gnome-text-editor".into()) },
            Installed { id: "org.keepassxc.KeePassXC".into(), name: "KeePassXC".into(), wm_class: None, exec: Some("keepassxc".into()) },
        ]
    }

    fn setup(tag: &str) -> (PathBuf, Vec<Recent>) {
        let dir = std::env::temp_dir().join(format!("helix-agent-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["notes one.txt", "report.pdf", "vault.kdbx", "secret.txt"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        std::fs::write(dir.join("recently-used.xbel"), XBEL.replace("DIR", &dir.to_string_lossy())).unwrap();
        let items = read_xbel(&dir.join("recently-used.xbel"));
        (dir, items)
    }

    #[test]
    fn reads_the_bookmark_file() {
        let (dir, items) = setup("read");
        assert_eq!(items.len(), 6);
        let notes = items.iter().find(|i| i.uri.ends_with("notes%20one.txt")).unwrap();
        assert_eq!(notes.used, 1_795_078_800, "visited is the latest");
        assert_eq!(notes.apps, [("Text Editor".to_owned(), "gnome-text-editor".to_owned())]);
        assert!(items.iter().find(|i| i.uri.ends_with("secret.txt")).unwrap().private);
        assert!(read_xbel(&dir.join("missing.xbel")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_files() {
        let (dir, items) = setup("recent");
        let config = Config { enabled: true, excluded_apps: vec!["org.keepassxc.KeePassXC".into()], ..Config::default() };
        let files = recent(items.clone(), &installed(), &config, i64::MIN, None, 50);
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        // Newest first; the excluded app's, the private and the missing
        // ones left out; the remote one kept.
        assert_eq!(names, ["notes one.txt", "report.pdf", "plan.odt"]);
        assert_eq!(files[0].size, Some(1));
        assert_eq!(files[0].used, Some(1_795_078_800));
        assert_eq!(files[0].mime_type, "text/plain");
        assert!(files[2].size.is_none());
        let pdfs = recent(items.clone(), &installed(), &config, i64::MIN, Some("application/pdf"), 50);
        assert_eq!(pdfs.len(), 1);
        assert_eq!(recent(items.clone(), &installed(), &config, i64::MIN, Some("text/*"), 50).len(), 1);
        assert_eq!(recent(items.clone(), &installed(), &config, i64::MIN, None, 1).len(), 1);
        // The privacy settings' age.
        assert_eq!(recent(items.clone(), &installed(), &config, 1_795_000_000, None, 50).len(), 1);
        // Without the exclusion, the vault shows.
        let open = Config { enabled: true, ..Config::default() };
        assert!(recent(items, &installed(), &open, i64::MIN, None, 50).iter().any(|f| f.name == "vault.kdbx"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registration() {
        let (dir, items) = setup("apps");
        let notes = items.iter().find(|i| i.uri.ends_with("notes%20one.txt")).unwrap();
        assert!(registered_by_window(&installed(), "org.gnome.TextEditor", notes), "by the app's name and command");
        assert!(!registered_by_window(&installed(), "org.gnome.Papers", notes));
        let pdf = items.iter().find(|i| i.uri.ends_with("report.pdf")).unwrap();
        assert!(registered_by_window(&[], "org.gnome.Papers", pdf), "by the app id it registered with");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_and_options() {
        assert_eq!(display_name("file:///tmp/My%20File.txt"), "My File.txt");
        assert!(mime_matches("image/png", "image/*") && !mime_matches("text/plain", "image/*"));
        assert!(mime_matches("text/x-csrc", "text/plain"), "subtypes count");
        let f = FileInfo { uri: "file:///a".into(), name: "a".into(), mime_type: "text/plain".into(), modified: 5, size: Some(3), used: None };
        let r = f.record();
        assert_eq!(u64::try_from(r["size"].clone()).unwrap(), 3);
        assert!(!r.contains_key("used"));
        let opts = HashMap::from([("limit".to_owned(), value(100000u32)), ("in".to_owned(), value("/"))]);
        let s = search_options("report", &opts).unwrap();
        assert_eq!((s.limit, s.folder.as_path()), (MAX_FILES, Path::new("/")));
        assert_eq!(search_options(" ", &opts).unwrap_err().kind(), "InvalidArgs");
        let rel = HashMap::from([("in".to_owned(), value("Documents"))]);
        assert_eq!(search_options("x", &rel).unwrap_err().kind(), "InvalidArgs");
        let gone = HashMap::from([("in".to_owned(), value("/no/such/folder"))]);
        assert_eq!(search_options("x", &gone).unwrap_err().kind(), "NotFound");
        let bad = HashMap::from([("since".to_owned(), value("yesterday"))]);
        assert_eq!(search_options("x", &bad).unwrap_err().kind(), "InvalidArgs");
        assert_eq!(limit(&HashMap::new()).unwrap(), DEFAULT_FILES);
    }
}
