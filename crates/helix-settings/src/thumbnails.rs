//! File previews (helix-desktop-spec `contracts/dbus/org.freedesktop.impl.portal.md`,
//! "FileChooser"): text files, pictures, and other files through the
//! freedesktop thumbnail cache and the installed thumbnailers.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gio::prelude::*;

/// The size thumbnailers are asked for (the "large" size).
const SIZE: u32 = 256;
/// How long a thumbnailer may run.
const TIMEOUT: Duration = Duration::from_secs(10);

/// A file's MIME type, from its name and first bytes.
pub fn mime_of(path: &Path) -> String {
    let mut head = vec![0u8; 4096];
    let n = std::fs::File::open(path).and_then(|mut f| std::io::Read::read(&mut f, &mut head)).unwrap_or(0);
    head.truncate(n);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    let (guess, _) = gio::content_type_guess(name.as_deref(), (!head.is_empty()).then_some(head.as_slice()));
    gio::content_type_get_mime_type(&guess).map(|m| m.to_string()).unwrap_or_default()
}

/// Pictures Helix decodes itself.
pub fn is_picture(mime: &str) -> bool {
    ["image/png", "image/jpeg", "image/webp", "image/jxl", "image/svg+xml"].contains(&mime)
}

/// Text that can be shown as it is.
pub fn is_text(mime: &str) -> bool {
    let type_ = gio::content_type_from_mime_type(mime);
    mime.starts_with("text/") || type_.is_some_and(|t| gio::content_type_is_a(&t, "text/plain"))
}

#[derive(Debug, Clone)]
struct Thumbnailer {
    mimes: Vec<String>,
    exec: String,
}

/// `thumbnailers/*.thumbnailer` in the data folders (user's first; a
/// thumbnailer whose `TryExec` is missing is skipped).
fn thumbnailers() -> &'static [Thumbnailer] {
    static LIST: OnceLock<Vec<Thumbnailer>> = OnceLock::new();
    LIST.get_or_init(|| {
        let mut dirs = vec![gio::glib::user_data_dir()];
        dirs.extend(gio::glib::system_data_dirs());
        let mut out = Vec::new();
        for dir in dirs {
            let Ok(read) = std::fs::read_dir(dir.join("thumbnailers")) else { continue };
            for entry in read.flatten() {
                let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
                let get = |key: &str| text.lines().find_map(|l| l.strip_prefix(key).and_then(|r| r.trim_start().strip_prefix('='))).map(|v| v.trim().to_owned());
                let (Some(exec), Some(mimes)) = (get("Exec"), get("MimeType")) else { continue };
                if get("TryExec").is_some_and(|t| gio::glib::find_program_in_path(&t).is_none() && !Path::new(&t).is_file()) {
                    continue;
                }
                out.push(Thumbnailer { mimes: mimes.split(';').filter(|m| !m.is_empty()).map(str::to_owned).collect(), exec });
            }
        }
        out
    })
}

fn thumbnailer_for(mime: &str) -> Option<&'static Thumbnailer> {
    thumbnailers().iter().find(|t| t.mimes.iter().any(|m| m == mime))
}

/// Whether `path` can be previewed: a picture, text, or a type a
/// thumbnailer handles.
pub fn can_preview(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let mime = mime_of(path);
    is_picture(&mime) || is_text(&mime) || thumbnailer_for(&mime).is_some()
}

fn uri_of(path: &Path) -> String {
    gio::File::for_path(path).uri().to_string()
}

/// The thumbnail file name for `uri` (the MD5 of the URI, `.png`).
pub fn cache_name(uri: &str) -> String {
    let sum = gio::glib::compute_checksum_for_string(gio::glib::ChecksumType::Md5, uri).map(|s| s.to_string()).unwrap_or_default();
    format!("{sum}.png")
}

fn modified(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Whether a fresh thumbnail of `path` is cached already.
pub fn thumbnail_known(path: &Path) -> bool {
    let Some(file_time) = modified(path) else { return false };
    let name = cache_name(&uri_of(path));
    let cache = gio::glib::user_cache_dir();
    ["thumbnails/x-large", "thumbnails/large", "thumbnails/normal", "helix/thumbnails"].iter().any(|d| modified(&cache.join(d).join(&name)).is_some_and(|t| t >= file_time))
}

/// A thumbnail at least as new as the file, in the shared cache or
/// Helix's own; made with a thumbnailer when there is none.
pub fn thumbnail(path: &Path) -> Option<PathBuf> {
    let file_time = modified(path)?;
    let uri = uri_of(path);
    let name = cache_name(&uri);
    let cache = gio::glib::user_cache_dir();
    let fresh = |p: &Path| modified(p).is_some_and(|t| t >= file_time);
    for size in ["x-large", "large", "normal"] {
        let p = cache.join("thumbnails").join(size).join(&name);
        if fresh(&p) {
            return Some(p);
        }
    }
    let own = cache.join("helix").join("thumbnails").join(&name);
    if fresh(&own) {
        return Some(own);
    }
    let thumbnailer = thumbnailer_for(&mime_of(path))?;
    std::fs::create_dir_all(own.parent()?).ok()?;
    let argv = gio::glib::shell_parse_argv(&thumbnailer.exec).ok()?;
    let args: Vec<String> = argv
        .iter()
        .map(|a| {
            let a = a.to_string_lossy();
            a.replace("%i", &path.to_string_lossy()).replace("%u", &uri).replace("%o", &own.to_string_lossy()).replace("%s", &SIZE.to_string()).replace("%%", "%")
        })
        .collect();
    let (program, rest) = args.split_first()?;
    let mut child = std::process::Command::new(program).args(rest).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::info!(program, "thumbnailer took too long");
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
    own.is_file().then_some(own)
}

/// The first `lines` lines of a text file (at most 8 KiB read), tabs as
/// spaces.
pub fn text_lines(path: &Path, lines: usize) -> Vec<String> {
    let mut buf = vec![0u8; 8192];
    let n = std::fs::File::open(path).and_then(|mut f| std::io::Read::read(&mut f, &mut buf)).unwrap_or(0);
    buf.truncate(n);
    String::from_utf8_lossy(&buf).lines().take(lines).map(|l| l.replace('\t', "    ")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_kinds() {
        // The thumbnail spec's own example.
        assert_eq!(cache_name("file:///home/jens/photos/me.png"), "c6ee772d9e49320e97ec29a7eb5b1697.png");
        let dir = std::env::temp_dir().join(format!("helix-thumbs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let notes = dir.join("notes");
        std::fs::write(&notes, "first line\n\tsecond\nthird\n").unwrap();
        assert!(is_text(&mime_of(&notes)), "{}", mime_of(&notes));
        assert!(can_preview(&notes));
        assert_eq!(text_lines(&notes, 2), ["first line", "    second"]);
        assert!(!can_preview(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
