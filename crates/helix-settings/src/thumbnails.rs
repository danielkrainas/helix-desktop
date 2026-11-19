//! File previews (helixdesktop/spec `contracts/dbus/org.freedesktop.impl.portal.md`,
//! "FileChooser"): text files, pictures, and other files through the
//! freedesktop thumbnail cache and the installed thumbnailers.
//!
//! Thumbnailers parse untrusted files with large libraries, so, as in
//! GNOME, they run in bubblewrap ([`Sandbox`]): no network, no other
//! namespaces' processes, nothing of the user's but the file (read-only,
//! under a fixed name) and an empty folder for the thumbnail.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gio::prelude::*;

/// The size thumbnailers are asked for (the "large" size).
const SIZE: u32 = 256;
/// How long a thumbnailer may run.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Where a thumbnailer finds its input (plus the file's extension) and
/// writes its output, inside the sandbox.
const INPUT_INSIDE: &str = "/tmp/helix-thumbnail-input";
const OUTPUT_INSIDE: &str = "/tmp/helix-thumbnail-output";

/// How thumbnailers are run: inside bubblewrap.
#[derive(Debug, Clone)]
pub struct Sandbox {
    /// What starts bubblewrap: `bwrap`, or a helper that runs it (Helix's
    /// `helix-extension-sandbox`, which AppArmor lets use user
    /// namespaces; it adds `--unshare-user --disable-userns` itself).
    pub launcher: PathBuf,
    /// Turn user namespaces off inside (bubblewrap 0.8 and later).
    pub disable_userns: bool,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self { launcher: PathBuf::from("bwrap"), disable_userns: false }
    }
}

/// What the sandbox shows of the host.
#[derive(Debug, Clone, Default)]
struct Room {
    /// `/bin`, `/lib`… as they are: (path, where it links to, or `None`
    /// for a directory).
    root_entries: Vec<(PathBuf, Option<PathBuf>)>,
    /// The thumbnailer's program, when it is outside `/usr`.
    program: Option<PathBuf>,
    /// The user's fonts and font cache (read-only, when they exist).
    fonts: Vec<PathBuf>,
    lang: Option<String>,
}

impl Room {
    fn current(program: Option<PathBuf>) -> Self {
        let root_entries = ["/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32"]
            .iter()
            .filter_map(|p| {
                let path = PathBuf::from(p);
                let meta = std::fs::symlink_metadata(&path).ok()?;
                Some((path.clone(), if meta.file_type().is_symlink() { std::fs::read_link(&path).ok() } else { None }))
            })
            .collect();
        let home = gio::glib::home_dir();
        let fonts = [gio::glib::user_data_dir().join("fonts"), home.join(".fonts"), gio::glib::user_cache_dir().join("fontconfig")].into_iter().filter(|p| p.is_dir()).collect();
        Self { root_entries, program, fonts, lang: std::env::var("LANG").ok().filter(|l| !l.is_empty()) }
    }
}

/// The bubblewrap arguments running thumbnailer `command` (already with
/// the inside paths) on `input`, seen inside as `input_inside`, writing
/// into `output_dir`.
fn sandbox_args(sandbox: &Sandbox, room: &Room, input: &Path, input_inside: &str, output_dir: &Path, command: &[String]) -> Vec<String> {
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let mut out: Vec<String> = Vec::new();
    let mut push = |items: &[&str]| out.extend(items.iter().map(|i| i.to_string()));
    push(&["--die-with-parent", "--new-session", "--unshare-all"]);
    if sandbox.disable_userns {
        push(&["--unshare-user", "--disable-userns"]);
    }
    push(&["--clearenv", "--setenv", "PATH", "/usr/bin:/bin", "--setenv", "HOME", "/tmp", "--setenv", "GIO_USE_VFS", "local"]);
    if let Some(lang) = &room.lang {
        push(&["--setenv", "LANG", lang]);
    }
    push(&["--ro-bind", "/usr", "/usr", "--ro-bind", "/etc", "/etc"]);
    for (path, target) in &room.root_entries {
        match target {
            Some(t) => push(&["--symlink", &s(t), &s(path)]),
            None => push(&["--ro-bind", &s(path), &s(path)]),
        }
    }
    push(&["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
    if let Some(program) = &room.program {
        push(&["--ro-bind", &s(program), &s(program)]);
    }
    for f in &room.fonts {
        push(&["--ro-bind-try", &s(f), &s(f)]);
    }
    push(&["--ro-bind", &s(input), input_inside, "--bind", &s(output_dir), OUTPUT_INSIDE, "--chdir", "/", "--"]);
    out.extend(command.iter().cloned());
    out
}

/// The extension a file's inside name keeps (thumbnailers may look at
/// it): letters and digits only.
fn plain_extension(path: &Path) -> String {
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    if !ext.is_empty() && ext.len() <= 16 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        format!(".{ext}")
    } else {
        String::new()
    }
}

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

/// Whether an installed thumbnailer handles MIME type `mime`.
pub fn has_thumbnailer(mime: &str) -> bool {
    thumbnailer_for(mime).is_some()
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
/// Helix's own; made with a thumbnailer, in `sandbox`, when there is none.
pub fn thumbnail(path: &Path, sandbox: &Sandbox) -> Option<PathBuf> {
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
    let dir = own.parent()?;
    std::fs::create_dir_all(dir).ok()?;
    // A new folder only this user can enter for the thumbnailer's output.
    let output_dir = gio::glib::mkdtemp_full(dir.join(".new-XXXXXX"), 0o700)?;
    let made = run_thumbnailer(thumbnailer, path, &output_dir, sandbox);
    let result = made.and_then(|made| std::fs::rename(&made, &own).ok()).map(|()| own);
    let _ = std::fs::remove_dir_all(&output_dir);
    result
}

/// Run `thumbnailer` on `path` in `sandbox`; its thumbnail in
/// `output_dir`, if it made one that is a PNG.
fn run_thumbnailer(thumbnailer: &Thumbnailer, path: &Path, output_dir: &Path, sandbox: &Sandbox) -> Option<PathBuf> {
    let input_inside = format!("{INPUT_INSIDE}{}", plain_extension(path));
    let output_inside = format!("{OUTPUT_INSIDE}/thumbnail.png");
    let argv = gio::glib::shell_parse_argv(&thumbnailer.exec).ok()?;
    let command: Vec<String> = argv
        .iter()
        .map(|a| {
            let a = a.to_string_lossy();
            a.replace("%i", &input_inside).replace("%u", &format!("file://{input_inside}")).replace("%o", &output_inside).replace("%s", &SIZE.to_string()).replace("%%", "%")
        })
        .collect();
    let program = command.first()?;
    // A program outside /usr is shown read-only inside.
    let found = if Path::new(program).is_absolute() { Some(PathBuf::from(program)) } else { gio::glib::find_program_in_path(program) };
    let outside = found.filter(|p| !p.starts_with("/usr/"));
    let args = sandbox_args(sandbox, &Room::current(outside), path, &input_inside, output_dir, &command);
    let mut child = std::process::Command::new(&sandbox.launcher).args(&args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().map_err(|err| tracing::warn!(%err, "cannot start the thumbnailer sandbox")).ok()?;
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
    let made = output_dir.join("thumbnail.png");
    // Only a regular file that is a PNG is taken.
    let meta = std::fs::symlink_metadata(&made).ok()?;
    let mut magic = [0u8; 8];
    let png = meta.is_file() && std::fs::File::open(&made).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut magic)).is_ok() && magic == *b"\x89PNG\r\n\x1a\n";
    png.then_some(made)
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

    fn has_seq(args: &[String], seq: &[&str]) -> bool {
        args.windows(seq.len()).any(|w| w.iter().zip(seq).all(|(a, b)| a == b))
    }

    #[test]
    fn thumbnailers_are_closed_in() {
        let room = Room { root_entries: vec![("/bin".into(), Some("usr/bin".into())), ("/lib64".into(), None)], program: Some("/opt/t/thumb".into()), fonts: vec!["/home/u/.local/share/fonts".into()], lang: Some("de_DE.UTF-8".into()) };
        let sandbox = Sandbox { launcher: "bwrap".into(), disable_userns: true };
        let command = ["thumb".to_owned(), "file:///tmp/helix-thumbnail-input.pdf".to_owned()];
        let a = sandbox_args(&sandbox, &room, Path::new("/home/u/Downloads/x y.pdf"), "/tmp/helix-thumbnail-input.pdf", Path::new("/home/u/.cache/helix/thumbnails/.new-1"), &command);
        assert!(has_seq(&a, &["--die-with-parent", "--new-session", "--unshare-all", "--unshare-user", "--disable-userns", "--clearenv"]));
        assert!(!a.iter().any(|x| x == "--share-net"), "no network");
        assert!(has_seq(&a, &["--setenv", "GIO_USE_VFS", "local"]) && has_seq(&a, &["--setenv", "LANG", "de_DE.UTF-8"]));
        assert!(has_seq(&a, &["--symlink", "usr/bin", "/bin"]) && has_seq(&a, &["--ro-bind", "/lib64", "/lib64"]));
        assert!(has_seq(&a, &["--ro-bind", "/opt/t/thumb", "/opt/t/thumb"]) && has_seq(&a, &["--ro-bind-try", "/home/u/.local/share/fonts", "/home/u/.local/share/fonts"]));
        // The file read-only under a fixed name; only the output folder writable.
        assert!(has_seq(&a, &["--ro-bind", "/home/u/Downloads/x y.pdf", "/tmp/helix-thumbnail-input.pdf"]));
        assert!(has_seq(&a, &["--bind", "/home/u/.cache/helix/thumbnails/.new-1", OUTPUT_INSIDE]));
        assert_eq!(a.iter().filter(|x| *x == "--bind").count(), 1);
        assert!(!a.iter().any(|x| x == "/home/u" || x.contains("/run/user") || x.contains("bus")), "nothing else of the user's");
        assert!(has_seq(&a, &["--chdir", "/", "--", "thumb", "file:///tmp/helix-thumbnail-input.pdf"]));
        let old = sandbox_args(&Sandbox::default(), &Room::default(), Path::new("/a"), "/tmp/i", Path::new("/o"), &command);
        assert!(!old.iter().any(|x| x == "--disable-userns"));
        assert_eq!(plain_extension(Path::new("a/b.PDF")), ".PDF");
        assert_eq!(plain_extension(Path::new("a/b.p df")), "");
        assert_eq!(plain_extension(Path::new("a/b")), "");
    }

    /// A real thumbnailer in the sandbox, where bubblewrap and GNOME's
    /// font thumbnailer are installed.
    #[test]
    fn a_thumbnailer_runs_in_the_sandbox() {
        let font = Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf");
        if gio::glib::find_program_in_path("bwrap").is_none() || gio::glib::find_program_in_path("gnome-thumbnail-font").is_none() || !font.is_file() {
            return;
        }
        let out = std::env::temp_dir().join(format!("helix-thumbs-sandbox-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        let font_thumbnailer = Thumbnailer { mimes: vec![], exec: "gnome-thumbnail-font --size %s %u %o".into() };
        let made = run_thumbnailer(&font_thumbnailer, font, &out, &Sandbox { launcher: "bwrap".into(), disable_userns: true });
        assert_eq!(made, Some(out.join("thumbnail.png")));
        // One that tries to reach the user's files finds nothing, and
        // writes nothing but its output folder.
        let home = gio::glib::home_dir();
        let snoop = Thumbnailer { mimes: vec![], exec: format!("sh -c 'ls {} > %o || touch /tmp/helix-thumbnail-output/thumbnail.png'", home.display()) };
        let _ = std::fs::remove_file(out.join("thumbnail.png"));
        assert_eq!(run_thumbnailer(&snoop, font, &out, &Sandbox { launcher: "bwrap".into(), disable_userns: true }), None, "an empty file is not a PNG");
        assert_eq!(std::fs::metadata(out.join("thumbnail.png")).map(|m| m.len()).ok(), Some(0), "the home folder is not there");
        let _ = std::fs::remove_dir_all(&out);
    }
}
