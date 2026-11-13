//! Removing old Trash and temporary files (helix-desktop-spec
//! `contracts/gsettings/org.gnome.desktop.privacy.md`, "Old files"), as
//! `org.gnome.desktop.privacy` `remove-old-trash-files`,
//! `remove-old-temp-files` and `old-files-age` ask.
//!
//! Only the home Trash and the user's own regular files (and emptied
//! directories) in the temporary folder are touched.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const SCHEMA: &str = "org.gnome.desktop.privacy";
/// The first clean-up after the shell starts, then one per hour.
const FIRST: Duration = Duration::from_secs(60);
const EVERY: Duration = Duration::from_secs(3600);

/// The settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Settings {
    pub trash: bool,
    pub temp: bool,
    /// Days.
    pub age: u32,
}

pub fn read() -> Settings {
    use gio::prelude::*;
    if !helix_settings::has_schema(SCHEMA) {
        return Settings::default();
    }
    let s = gio::Settings::new(SCHEMA);
    let has = |k: &str| s.settings_schema().is_some_and(|schema| schema.has_key(k));
    Settings {
        trash: has("remove-old-trash-files") && s.boolean("remove-old-trash-files"),
        temp: has("remove-old-temp-files") && s.boolean("remove-old-temp-files"),
        age: if has("old-files-age") { s.uint("old-files-age") } else { 30 },
    }
}

/// What was used within this many seconds stays, whatever `old-files-age`
/// says (0 would otherwise take files in use).
const KEEP_RECENT: i64 = 3600;

/// The newest time an entry may have been used and still go.
fn cutoff(age: u32, now: i64) -> i64 {
    (now - i64::from(age) * 86_400).min(now - KEEP_RECENT)
}

/// Unix seconds.
fn unix(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// A `.trashinfo` file's `DeletionDate` (local time,
/// `YYYY-MM-DDThh:mm:ss`) as Unix seconds.
pub fn deletion_date(info: &str) -> Option<i64> {
    let value = info.lines().find_map(|l| l.trim().strip_prefix("DeletionDate="))?.trim();
    let (date, time) = value.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i32>().ok());
    let (y, mo, da) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.split(':').map(|p| p.parse::<i32>().ok());
    let (h, mi, se) = (t.next()??, t.next()??, t.next()??);
    let local = gio::glib::DateTime::new(&gio::glib::TimeZone::local(), y, mo, da, h, mi, f64::from(se)).ok()?;
    Some(local.to_unix())
}

/// Removes what was trashed more than `age` days before `now` from the
/// Trash at `trash` (its `info` and `files`); returns how many items went.
pub fn clean_trash(trash: &Path, age: u32, now: i64) -> usize {
    let cutoff = cutoff(age, now);
    let Ok(infos) = std::fs::read_dir(trash.join("info")) else { return 0 };
    let mut removed = 0;
    for info in infos.flatten() {
        let path = info.path();
        if path.extension().is_none_or(|e| e != "trashinfo") {
            continue;
        }
        let when = std::fs::read_to_string(&path).ok().and_then(|t| deletion_date(&t)).or_else(|| info.metadata().ok().and_then(|m| m.modified().ok()).map(unix));
        if when.is_none_or(|w| w >= cutoff) {
            continue;
        }
        let Some(name) = path.file_stem() else { continue };
        let item = trash.join("files").join(name);
        let gone = match std::fs::symlink_metadata(&item) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&item),
            Ok(_) => std::fs::remove_file(&item),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        };
        match gone.and_then(|()| std::fs::remove_file(&path)) {
            Ok(()) => removed += 1,
            Err(err) => tracing::debug!(%err, item = %item.display(), "cannot remove an old trashed item"),
        }
    }
    removed
}

/// The latest of an entry's modification, access and change times.
fn last_used(m: &std::fs::Metadata) -> i64 {
    m.mtime().max(m.atime()).max(m.ctime())
}

/// Removes, below `dir`, the regular files and symbolic links owned by
/// `uid` not used for `age` days, and directories left empty that are as
/// old; other kinds of files (sockets, pipes, devices) are left. Returns
/// how many entries went.
pub fn clean_temp(dir: &Path, uid: u32, age: u32, now: i64) -> usize {
    let cutoff = cutoff(age, now);
    let mut removed = 0;
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(m) = std::fs::symlink_metadata(&path) else { continue };
        if m.uid() != uid {
            continue;
        }
        let ft = m.file_type();
        if ft.is_dir() {
            removed += clean_temp(&path, uid, age, now);
            // Emptied (its times changed only if something went).
            let empty = std::fs::read_dir(&path).is_ok_and(|mut d| d.next().is_none());
            if empty && last_used(&m) < cutoff && std::fs::remove_dir(&path).is_ok() {
                removed += 1;
            }
        } else if (ft.is_file() || ft.is_symlink()) && last_used(&m) < cutoff && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// The home Trash.
fn home_trash() -> PathBuf {
    gio::glib::user_data_dir().join("Trash")
}

/// One clean-up as `settings` ask.
pub fn run_once(settings: Settings) {
    let now = unix(SystemTime::now());
    if settings.trash {
        let n = clean_trash(&home_trash(), settings.age, now);
        tracing::info!(removed = n, age = settings.age, "old Trash items removed");
    }
    if settings.temp {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let dir = gio::glib::tmp_dir();
        let n = clean_temp(&dir, uid, settings.age, now);
        tracing::info!(removed = n, age = settings.age, dir = %dir.display(), "old temporary files removed");
    }
}

/// Cleans up a minute after start, then hourly, and at once when the
/// settings change after that (while one of them is on).
pub async fn run() {
    if !helix_settings::has_schema(SCHEMA) {
        return;
    }
    let watched = helix_settings::GSettings::new(&[SCHEMA]);
    let (mut changes, _watch) = watched.watch();
    let mut last = read();
    let mut started = false;
    let mut next = tokio::time::Instant::now() + FIRST;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(next) => {
                started = true;
                next = tokio::time::Instant::now() + EVERY;
                let settings = read();
                last = settings;
                if settings.trash || settings.temp {
                    let _ = tokio::task::spawn_blocking(move || run_once(settings)).await;
                }
            }
            change = changes.recv() => {
                if change.is_none() {
                    return;
                }
                let settings = read();
                if started && settings != last && (settings.trash || settings.temp) {
                    next = tokio::time::Instant::now();
                }
                last = settings;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("helix-housekeeping-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn set_times(path: &Path, unix_secs: i64) {
        let t = std::fs::FileTimes::new()
            .set_accessed(UNIX_EPOCH + Duration::from_secs(unix_secs as u64))
            .set_modified(UNIX_EPOCH + Duration::from_secs(unix_secs as u64));
        std::fs::File::options().write(true).open(path).or_else(|_| std::fs::File::open(path)).unwrap().set_times(t).unwrap();
    }

    #[test]
    fn the_last_hour_stays() {
        assert_eq!(cutoff(0, 10_000), 10_000 - 3600);
        assert_eq!(cutoff(1, 100_000), 100_000 - 86_400);
    }

    #[test]
    fn deletion_dates() {
        let info = "[Trash Info]\nPath=/home/u/a.txt\nDeletionDate=2026-01-02T03:04:05\n";
        let want = gio::glib::DateTime::new(&gio::glib::TimeZone::local(), 2026, 1, 2, 3, 4, 5.0).unwrap().to_unix();
        assert_eq!(deletion_date(info), Some(want));
        assert_eq!(deletion_date("[Trash Info]\nPath=/x\n"), None);
    }

    #[test]
    fn old_trash_goes() {
        let trash = temp_dir("trash");
        std::fs::create_dir_all(trash.join("info")).unwrap();
        std::fs::create_dir_all(trash.join("files/folder")).unwrap();
        std::fs::write(trash.join("files/folder/inner"), "x").unwrap();
        std::fs::write(trash.join("files/new.txt"), "x").unwrap();
        let now = gio::glib::DateTime::new(&gio::glib::TimeZone::local(), 2026, 3, 1, 12, 0, 0.0).unwrap().to_unix();
        std::fs::write(trash.join("info/folder.trashinfo"), "[Trash Info]\nPath=/f\nDeletionDate=2026-01-01T12:00:00\n").unwrap();
        std::fs::write(trash.join("info/new.txt.trashinfo"), "[Trash Info]\nPath=/n\nDeletionDate=2026-02-25T12:00:00\n").unwrap();
        assert_eq!(clean_trash(&trash, 30, now), 1);
        assert!(!trash.join("files/folder").exists() && !trash.join("info/folder.trashinfo").exists());
        assert!(trash.join("files/new.txt").exists() && trash.join("info/new.txt.trashinfo").exists());
        let _ = std::fs::remove_dir_all(&trash);
    }

    #[test]
    fn old_temp_files_go() {
        use std::os::unix::fs::MetadataExt;
        let dir = temp_dir("tmp");
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let now = unix(SystemTime::now());
        // Times can be set in the past, but the change time is now: the
        // clean-up is run as if `now` were 40 days later.
        let later = now + 40 * 86_400;
        std::fs::write(dir.join("old"), "x").unwrap();
        set_times(&dir.join("old"), now - 50 * 86_400);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/old"), "x").unwrap();
        std::fs::write(dir.join("recent"), "x").unwrap();
        let socket = std::os::unix::net::UnixListener::bind(dir.join("sock")).unwrap();
        assert_eq!(clean_temp(&dir, uid, 30, now), 0, "nothing old yet");
        // Used ten days before `later`: stays.
        set_times(&dir.join("recent"), later - 10 * 86_400);
        let removed = clean_temp(&dir, uid, 30, later);
        assert!(!dir.join("old").exists() && !dir.join("sub").exists());
        assert!(dir.join("recent").exists(), "used recently");
        assert!(dir.join("sock").exists(), "sockets are left");
        assert_eq!(removed, 3);
        assert_eq!(std::fs::metadata(&dir).unwrap().uid(), uid);
        assert_eq!(clean_temp(&dir, uid + 1, 0, later + 86_400 * 1000), 0, "only the user's own files");
        drop(socket);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
