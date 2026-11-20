//! `SetWallpaper`'s picture (helixdesktop/spec
//! `contracts/agent-access/README.md`, "Changing settings", "The
//! background" and "The copy"; helixdesktop/desktop#157).
//!
//! The file is opened once, following symlinks, and every check is of the
//! file opened: its real path is read from the open file (so a symlink or
//! a file swapped in after the check changes nothing), it must be in a
//! place [`allowed`] lets agents use, a regular file of at most
//! [`MAX_BYTES`], and a picture the background draws, by its content.
//! The copy is made from what was read, into
//! `$XDG_DATA_HOME/backgrounds/`, as the Wallpaper portal does.

use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::policy::AgentError;

/// The largest picture `SetWallpaper` takes.
pub const MAX_BYTES: u64 = 64 << 20;
/// Folders anyone may write to, where an agent's picture of the user's
/// own may be (pending decision (c): part of [`allowed`]).
pub const TEMP_DIRS: &[&str] = &["/tmp", "/var/tmp"];
/// Where removable drives the user mounted are, below a folder of the
/// user's name.
pub const REMOVABLE_DIRS: &[&str] = &["/media", "/run/media"];

/// The folders [`allowed`] judges by.
#[derive(Debug, Clone)]
pub struct Places {
    /// The home folder and `$XDG_DATA_HOME`, as real paths.
    pub home: PathBuf,
    pub data_home: PathBuf,
    /// `$XDG_DATA_DIRS`, as real paths.
    pub data_dirs: Vec<PathBuf>,
    /// The user's login name and id.
    pub user: String,
    pub uid: u32,
}

/// `path` with symlinks resolved, or as it is when that fails.
fn real(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

impl Places {
    /// This session's.
    pub fn current() -> Places {
        // SAFETY: getuid cannot fail.
        let uid = unsafe { libc::getuid() };
        Places {
            home: real(&gio::glib::home_dir()),
            data_home: real(&gio::glib::user_data_dir()),
            data_dirs: gio::glib::system_data_dirs().iter().map(|d| real(d)).collect(),
            user: gio::glib::user_name().to_string_lossy().into_owned(),
            uid,
        }
    }

    /// `$XDG_DATA_HOME/backgrounds`.
    pub fn backgrounds(&self) -> PathBuf {
        self.data_home.join("backgrounds")
    }

    /// Whether `real` is in a backgrounds folder: the user's or a system
    /// one (used where it is, not copied).
    pub fn in_backgrounds(&self, real: &Path) -> bool {
        real.starts_with(self.backgrounds()) || self.data_dirs.iter().any(|d| real.starts_with(d.join("backgrounds")))
    }
}

/// *Helix's choice* (pending decision (c), README.md, "Changing settings",
/// "The background"): whether an agent may make the picture at real path
/// `real`, owned by `owner`, the background:
/// - in the home folder, when no folder below it on the path, and not the
///   file itself, has a name starting with `.` (apps keep private data in
///   hidden folders: `~/.config`, `~/.cache`, `~/.var/app`, `~/.mozilla`);
///   or anywhere in `$XDG_DATA_HOME/backgrounds/`;
/// - in a system backgrounds folder (`<dir>/backgrounds/` of
///   `$XDG_DATA_DIRS`);
/// - in [`TEMP_DIRS`], owned by the user;
/// - on a removable drive the user mounted ([`REMOVABLE_DIRS`]`/<user>/`).
pub fn allowed(real: &Path, owner: u32, places: &Places) -> bool {
    if places.in_backgrounds(real) {
        return true;
    }
    if let Ok(rest) = real.strip_prefix(&places.home) {
        return !rest.as_os_str().is_empty() && !rest.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.'));
    }
    if TEMP_DIRS.iter().any(|t| real.starts_with(t)) {
        return owner == places.uid;
    }
    !places.user.is_empty() && REMOVABLE_DIRS.iter().any(|d| real.starts_with(Path::new(d).join(&places.user)))
}

/// A picture as it was opened: its real path and its content.
#[derive(Debug)]
pub struct Picture {
    pub real: PathBuf,
    pub data: Vec<u8>,
}

/// `path` without the ` (deleted)` the kernel adds to an unlinked file's.
fn without_deleted(link: PathBuf) -> PathBuf {
    match link.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(trimmed) => PathBuf::from(trimmed),
        None => link,
    }
}

/// Open the picture at `path` for `SetWallpaper`: `NotFound` when there
/// is nothing there, `Excluded` (`private-file`) when its real path is
/// not [`allowed`] (checked before anything of it is read), `NotSupported`
/// when it is not a regular file of at most [`MAX_BYTES`] that the
/// background draws (PNG, JPEG, WebP, JPEG XL, SVG; not a slideshow).
pub fn open(path: &Path, places: &Places) -> Result<Picture, AgentError> {
    use std::os::fd::AsRawFd;
    let not_picture = || AgentError::NotSupported("not a picture".into());
    // Non-blocking: opening a FIFO must not wait for a writer.
    let file = match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(AgentError::NotFound(path.display().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            // Not the user's to read (or in a folder that is not): where
            // is it?
            let real = real(path);
            let owner = std::fs::metadata(&real).map(|m| m.uid()).unwrap_or(u32::MAX);
            if !allowed(&real, owner, places) {
                return Err(AgentError::Excluded("private-file".into()));
            }
            return Err(not_picture());
        }
        Err(_) => return Err(not_picture()),
    };
    let meta = file.metadata().map_err(|_| not_picture())?;
    let real = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).map(without_deleted).map_err(|_| AgentError::Failed("cannot tell where the file is".into()))?;
    if !allowed(&real, meta.uid(), places) {
        return Err(AgentError::Excluded("private-file".into()));
    }
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return Err(not_picture());
    }
    let mut data = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_BYTES + 1).read_to_end(&mut data).map_err(|_| not_picture())?;
    if data.len() as u64 > MAX_BYTES {
        return Err(not_picture());
    }
    // Drawn whole once, as the background would.
    crate::wallpaper::decode_bytes(&data, &real).map_err(|_| not_picture())?;
    Ok(Picture { real, data })
}

/// The copy's name for a picture named `original` copied at `stamp`
/// (`YYYY-MM-DD-HH-MM-SS`), with `n` added before the extension when the
/// name is taken (`n` > 0).
pub fn copy_name(stamp: &str, original: &str, n: u32) -> String {
    let original = if original.is_empty() { "background" } else { original };
    if n == 0 {
        return format!("{stamp}-{original}");
    }
    match original.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()) {
        Some((stem, ext)) => format!("{stamp}-{stem}-{n}.{ext}"),
        None => format!("{stamp}-{original}-{n}"),
    }
}

/// Where `picture` is used from: where it is when it is in a backgrounds
/// folder, else a copy (mode 0600) of what was read, in
/// `$XDG_DATA_HOME/backgrounds/`. Its `file://` URI.
pub fn place(picture: &Picture, places: &Places, stamp: &str) -> Result<String, AgentError> {
    use std::io::Write;
    let uri = |p: &Path| gio::glib::filename_to_uri(p, None).map(|u| u.to_string()).map_err(|e| AgentError::Failed(e.to_string()));
    if places.in_backgrounds(&picture.real) {
        return uri(&picture.real);
    }
    let dir = places.backgrounds();
    std::fs::create_dir_all(&dir).map_err(|e| AgentError::Failed(format!("cannot make {}: {e}", dir.display())))?;
    let original = picture.real.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    for n in 0..1000 {
        let target = dir.join(copy_name(stamp, &original, n));
        let file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&target);
        match file {
            Ok(mut file) => {
                if let Err(e) = file.write_all(&picture.data).and_then(|()| file.sync_all()) {
                    let _ = std::fs::remove_file(&target);
                    return Err(AgentError::Failed(format!("cannot copy the picture: {e}")));
                }
                return uri(&target);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(AgentError::Failed(format!("cannot copy the picture: {e}"))),
        }
    }
    Err(AgentError::Failed("cannot name the copy".into()))
}

/// Now as the copy's stamp.
pub fn stamp() -> String {
    gio::glib::DateTime::now_local().and_then(|d| d.format("%Y-%m-%d-%H-%M-%S")).map(|s| s.to_string()).unwrap_or_else(|_| "0000-00-00-00-00-00".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places(root: &Path) -> Places {
        Places {
            home: root.join("home"),
            data_home: root.join("home/.local/share"),
            data_dirs: vec![root.join("usr/share")],
            user: "me".into(),
            uid: 1000,
        }
    }

    #[test]
    fn where_pictures_may_be() {
        let p = places(Path::new("/r"));
        let ok = |path: &str, owner: u32| allowed(Path::new(path), owner, &p);
        assert!(ok("/r/home/Pictures/sea.png", 1000));
        assert!(ok("/r/home/sea.png", 0));
        assert!(ok("/r/home/.local/share/backgrounds/a.png", 1000), "the user's backgrounds folder");
        assert!(ok("/r/home/.local/share/backgrounds/.x/a.png", 1000));
        assert!(ok("/r/usr/share/backgrounds/gnome/blobs.svg", 0));
        assert!(ok("/tmp/shot.png", 1000));
        assert!(ok("/var/tmp/a/b.png", 1000));
        assert!(ok("/media/me/USB/a.png", 0));
        assert!(ok("/run/media/me/USB/a.png", 0));
        for refused in [
            "/r/home/.config/app/secret.png",
            "/r/home/.cache/thumbnails/x.png",
            "/r/home/.var/app/org.example/data/a.png",
            "/r/home/.mozilla/firefox/a.png",
            "/r/home/Pictures/.hidden.png",
            "/r/home/Pictures/.private/a.png",
            "/r/home/.local/share/other/a.png",
            "/r/home",
            "/etc/shadow",
            "/proc/self/environ",
            "/run/user/1000/secret.png",
            "/home/other/a.png",
            "/r/usr/share/icons/a.png",
            "/media/other/USB/a.png",
        ] {
            assert!(!ok(refused, 1000), "{refused}");
        }
        assert!(!ok("/tmp/planted.png", 1001), "another user's file in /tmp");
        assert!(!ok("/tmpfoo/a.png", 1000));
    }

    #[test]
    fn copy_names() {
        assert_eq!(copy_name("2026-11-20-10-00-00", "sea.png", 0), "2026-11-20-10-00-00-sea.png");
        assert_eq!(copy_name("2026-11-20-10-00-00", "sea.png", 2), "2026-11-20-10-00-00-sea-2.png");
        assert_eq!(copy_name("2026-11-20-10-00-00", "README", 1), "2026-11-20-10-00-00-README-1");
        assert_eq!(copy_name("s", ".png", 1), "s-.png-1");
    }

    /// A tiny PNG.
    fn png() -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, 2, 2);
            enc.set_color(png::ColorType::Rgba);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[255; 16]).unwrap();
        }
        out
    }

    #[test]
    fn opened_once_and_copied_from_what_was_read() {
        let root = std::env::temp_dir().join(format!("helix-bg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        for d in ["Pictures", ".config/app", ".local/share"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        // SAFETY: getuid cannot fail.
        let uid = unsafe { libc::getuid() };
        let p = Places { home: home.clone(), data_home: home.join(".local/share"), data_dirs: vec![root.join("sys")], user: "me".into(), uid };
        let sea = home.join("Pictures/sea.png");
        std::fs::write(&sea, png()).unwrap();
        let secret = home.join(".config/app/secret.png");
        std::fs::write(&secret, png()).unwrap();
        let kind = |r: Result<Picture, AgentError>| r.map(|_| "ok".to_owned()).unwrap_or_else(|e| e.kind().to_string());
        assert_eq!(kind(open(&sea, &p)), "ok");
        assert_eq!(kind(open(&secret, &p)), "Excluded");
        assert_eq!(kind(open(&home.join("Pictures/none.png"), &p)), "NotFound");
        // A symlink is judged by what it points to, both ways.
        std::os::unix::fs::symlink(&secret, home.join("Pictures/innocent.png")).unwrap();
        assert_eq!(kind(open(&home.join("Pictures/innocent.png"), &p)), "Excluded");
        std::os::unix::fs::symlink(&sea, home.join(".config/app/link.png")).unwrap();
        assert_eq!(open(&home.join(".config/app/link.png"), &p).unwrap().real, sea);
        // Not a picture, a folder, too large.
        std::fs::write(home.join("Pictures/notes.png"), b"just text").unwrap();
        assert_eq!(kind(open(&home.join("Pictures/notes.png"), &p)), "NotSupported");
        std::fs::write(home.join("Pictures/show.xml"), b"<background><static><file>/x.png</file></static></background>").unwrap();
        assert_eq!(kind(open(&home.join("Pictures/show.xml"), &p)), "NotSupported", "a slideshow");
        assert_eq!(kind(open(&home.join("Pictures"), &p)), "NotSupported");
        // Swapped after it was opened: the copy is what was read.
        let picture = open(&sea, &p).unwrap();
        std::fs::remove_file(&sea).unwrap();
        std::fs::write(&sea, b"swapped").unwrap();
        let uri = place(&picture, &p, "2026-11-20-10-00-00").unwrap();
        let copy = gio::glib::filename_from_uri(&uri).unwrap().0;
        assert_eq!(copy, home.join(".local/share/backgrounds/2026-11-20-10-00-00-sea.png"));
        assert_eq!(std::fs::read(&copy).unwrap(), png());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&copy).unwrap().permissions().mode() & 0o777, 0o600);
        // The name taken: a number before the extension.
        let again = place(&picture, &p, "2026-11-20-10-00-00").unwrap();
        assert!(again.ends_with("2026-11-20-10-00-00-sea-1.png"), "{again}");
        // Already in the backgrounds folder: used where it is.
        let used = open(&copy, &p).unwrap();
        assert_eq!(place(&used, &p, "x").unwrap(), uri);
        let _ = std::fs::remove_dir_all(&root);
    }
}
