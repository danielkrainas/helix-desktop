//! Changing the login screen's configuration: `helix-login-config`, run
//! as root through `pkexec` (polkit action
//! `org.helixdesktop.login-screen-config`), takes the keys to change on
//! standard input, checks them, and writes them into
//! `/etc/helix/greeter.toml` ([`CONFIG_PATH`]).
//!
//! Spec: helixdesktop/spec `components/greeter.md`, "Changing the
//! configuration".
//!
//! The input is a TOML document with only the keys being changed, from
//! the keys the greeter reads ([`KEYS`]), and optionally `remove`, an
//! array of key names to delete. Anything else refuses the whole input.
//! The file is edited with `toml_edit`, so what the helper does not change
//! (keys it doesn't know, tables, comments) stays as it was; the values
//! written are made from the parsed input, never copied from it.
//!
//! The path is a parameter of [`install`] so that tests can use a scratch
//! directory; the binary always passes [`CONFIG_PATH`], and nothing in its
//! environment or arguments can change that.

use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use crate::config::{ACCENT, BANNER, CLOCK_FORMAT, CLOCK_FORMATS, KEYS, USER_LIST};
pub use crate::config::CONFIG_PATH;

/// The most input read, in bytes.
pub const MAX_INPUT: usize = 16 * 1024;
/// The longest banner, in characters (Unicode scalar values).
pub const MAX_BANNER: usize = 200;
/// The input key that names keys to delete.
pub const REMOVE: &str = "remove";
/// The largest existing file read; a larger one is left alone.
const MAX_FILE: u64 = 1024 * 1024;

/// Why nothing was written.
#[derive(Debug, PartialEq)]
pub enum Error {
    /// The input is not acceptable (exit status 1).
    Invalid(String),
    /// The configuration could not be read or written (exit status 2).
    Io(String),
}

impl Error {
    /// The helper's exit status for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Invalid(_) => 1,
            Error::Io(_) => 2,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Invalid(e) => write!(f, "invalid input: {e}"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

/// A value to set.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Str(String),
}

/// What the input asks for.
#[derive(Debug, Default, PartialEq)]
pub struct Changes {
    /// Keys to set, in the input's order.
    pub set: Vec<(&'static str, Value)>,
    /// Keys to delete.
    pub remove: Vec<&'static str>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.remove.is_empty()
    }
}

fn invalid(e: impl Into<String>) -> Error {
    Error::Invalid(e.into())
}

/// `s` quoted for a one-line message, shortened.
fn quoted(s: &str) -> String {
    let short: String = s.chars().take(40).collect();
    let ellipsis = if short.len() < s.len() { "…" } else { "" };
    format!("{short:?}{ellipsis}")
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn known(key: &str) -> Option<&'static str> {
    KEYS.into_iter().find(|k| *k == key)
}

/// A bidirectional override or isolate: LRE, RLE, PDF, LRO, RLO, LRI,
/// RLI, FSI, PDI. They could make the banner read differently from what
/// was typed.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// The banner's rules: not blank (remove it instead), at most
/// [`MAX_BANNER`] characters, no control characters (line breaks and tabs
/// included) and no bidirectional overrides or isolates.
pub fn check_banner(banner: &str) -> Result<(), Error> {
    if banner.trim().is_empty() {
        return Err(invalid("banner is empty; remove it instead"));
    }
    if banner.chars().count() > MAX_BANNER {
        return Err(invalid(format!("banner is longer than {MAX_BANNER} characters")));
    }
    if banner.chars().any(char::is_control) {
        return Err(invalid("banner contains a control character"));
    }
    if banner.chars().any(is_bidi_control) {
        return Err(invalid("banner contains a bidirectional override or isolate character"));
    }
    Ok(())
}

fn string<'a>(key: &str, value: &'a toml::Value) -> Result<&'a str, Error> {
    value.as_str().ok_or_else(|| invalid(format!("{key} must be a string")))
}

/// Check `input` and say what it changes.
pub fn parse(input: &[u8]) -> Result<Changes, Error> {
    if input.len() > MAX_INPUT {
        return Err(invalid(format!("larger than {} KiB", MAX_INPUT / 1024)));
    }
    let text = std::str::from_utf8(input).map_err(|_| invalid("not UTF-8"))?;
    let table: toml::Table = toml::from_str(text).map_err(|e| invalid(format!("not TOML: {}", one_line(e.message()))))?;
    let mut changes = Changes::default();
    let mut remove = None;
    for (key, value) in &table {
        if key == REMOVE {
            let names = value.as_array().ok_or_else(|| invalid("remove must be an array of key names"))?;
            let mut keys = Vec::new();
            for name in names {
                let name = name.as_str().ok_or_else(|| invalid("remove must be an array of key names"))?;
                let key = known(name).ok_or_else(|| invalid(format!("remove names an unknown key {}", quoted(name))))?;
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
            remove = Some(keys);
            continue;
        }
        let Some(key) = known(key) else {
            return Err(invalid(format!("unknown key {}", quoted(key))));
        };
        let value = match key {
            USER_LIST => Value::Bool(value.as_bool().ok_or_else(|| invalid("user-list must be true or false"))?),
            BANNER => {
                let banner = string(key, value)?;
                check_banner(banner)?;
                Value::Str(banner.to_owned())
            }
            ACCENT => {
                let name = string(key, value)?;
                if helix_draw::accent::Accent::from_name(name).is_none() {
                    let names: Vec<_> = helix_draw::accent::Accent::ALL.iter().map(|a| a.name()).collect();
                    return Err(invalid(format!("accent must be one of {}", names.join(", "))));
                }
                Value::Str(name.to_owned())
            }
            CLOCK_FORMAT => {
                let format = string(key, value)?;
                if !CLOCK_FORMATS.contains(&format) {
                    return Err(invalid(format!("clock-format must be {}", CLOCK_FORMATS.join(" or "))));
                }
                Value::Str(format.to_owned())
            }
            _ => unreachable!("every known key is handled"),
        };
        changes.set.push((key, value));
    }
    changes.remove = remove.unwrap_or_default();
    if let Some(key) = changes.remove.iter().find(|k| changes.set.iter().any(|(s, _)| s == *k)) {
        return Err(invalid(format!("{key} is both set and removed")));
    }
    Ok(changes)
}

/// `existing` (the file's contents) with `changes` made: the rest of the
/// document, comments included, stays as it was. A changed key keeps its
/// place and its comments.
pub fn apply(existing: &str, changes: &Changes, path: &Path) -> Result<String, Error> {
    let mut doc: toml_edit::DocumentMut = existing.parse().map_err(|e: toml_edit::TomlError| {
        Error::Io(format!("{} is not valid TOML ({}); it was left as it is", path.display(), one_line(e.message())))
    })?;
    for (key, value) in &changes.set {
        let mut new = match value {
            Value::Bool(b) => toml_edit::Value::from(*b),
            Value::Str(s) => toml_edit::Value::from(s.as_str()),
        };
        if let Some(toml_edit::Item::Value(old)) = doc.get(key) {
            *new.decor_mut() = old.decor().clone();
        }
        doc[*key] = toml_edit::Item::Value(new);
    }
    for key in &changes.remove {
        doc.remove(key);
    }
    Ok(doc.to_string())
}

/// The file at `target`; empty when there is none. A symbolic link,
/// anything but a regular file, or a file too large is refused.
fn read_existing(target: &Path) -> Result<String, Error> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(target)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(Error::Io(format!("{} is a symbolic link; it was left as it is", target.display())))
        }
        Err(e) => return Err(Error::Io(format!("cannot read {}: {e}", target.display()))),
    };
    let meta = file.metadata().map_err(|e| Error::Io(format!("cannot read {}: {e}", target.display())))?;
    if !meta.is_file() {
        return Err(Error::Io(format!("{} is not a regular file; it was left as it is", target.display())));
    }
    if meta.len() > MAX_FILE {
        return Err(Error::Io(format!("{} is larger than 1 MiB; it was left as it is", target.display())));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::Io(format!("cannot read {}: {e}", target.display())))?;
    String::from_utf8(bytes).map_err(|_| Error::Io(format!("{} is not UTF-8; it was left as it is", target.display())))
}

/// Check `input` and make its changes in `target` (the binary always
/// passes [`CONFIG_PATH`]). `target` is replaced atomically, mode 0644,
/// owned by root when run as root. Input with nothing to change writes
/// nothing.
pub fn install(input: &[u8], target: &Path) -> Result<(), Error> {
    let changes = parse(input)?;
    if changes.is_empty() {
        return Ok(());
    }
    let existing = read_existing(target)?;
    let text = apply(&existing, &changes, target)?;
    write_atomically(target, text.as_bytes()).map_err(|e| Error::Io(format!("cannot write {}: {e}", target.display())))
}

fn running_as_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Write `contents` to a new file next to `target`, flush it, rename it
/// over `target` (replacing a symbolic link there, never following it)
/// and flush the directory. The directory is created (0755) if missing;
/// its parent must exist.
fn write_atomically(target: &Path, contents: &[u8]) -> std::io::Result<()> {
    let dir = target.parent().ok_or_else(|| std::io::Error::other("no directory"))?;
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(std::io::Error::other(format!("{} is not a directory", dir.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new().mode(0o755).create(dir)?;
            // Whatever the umask.
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
            if running_as_root() {
                std::os::unix::fs::chown(dir, Some(0), Some(0))?;
            }
        }
        Err(e) => return Err(e),
    }
    let name = target.file_name().ok_or_else(|| std::io::Error::other("no file name"))?;
    let tmp = dir.join(format!(".{}.{}.tmp", name.to_string_lossy(), std::process::id()));
    // A leftover from an earlier run with the same process ID.
    let _ = std::fs::remove_file(&tmp);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o644))?;
        if running_as_root() {
            std::os::unix::fs::fchown(&file, Some(0), Some(0))?;
        }
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&tmp, target)?;
        std::fs::File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;

    fn refused(input: &str) -> String {
        match parse(input.as_bytes()) {
            Err(Error::Invalid(e)) => {
                assert!(!e.contains('\n'), "one line: {e}");
                e
            }
            other => panic!("{input:?} accepted: {other:?}"),
        }
    }

    #[test]
    fn accepts_the_keys() {
        let changes = parse(b"user-list = false\nbanner = \"Authorized use only.\"\naccent = \"teal\"\nclock-format = \"24h\"\n").unwrap();
        assert_eq!(
            changes.set,
            vec![
                (USER_LIST, Value::Bool(false)),
                (BANNER, Value::Str("Authorized use only.".into())),
                (ACCENT, Value::Str("teal".into())),
                (CLOCK_FORMAT, Value::Str("24h".into())),
            ]
        );
        assert!(changes.remove.is_empty());
        for accent in helix_draw::accent::Accent::ALL {
            assert!(parse(format!("accent = \"{}\"", accent.name()).as_bytes()).is_ok());
        }
        assert!(parse(b"clock-format = \"12h\"").is_ok());
        let changes = parse(b"remove = [\"banner\", \"clock-format\", \"banner\"]").unwrap();
        assert_eq!(changes.remove, vec![BANNER, CLOCK_FORMAT]);
        assert!(parse(b"remove = []").unwrap().is_empty());
        assert!(parse(b"").unwrap().is_empty());
        // Right-to-left text and its marks are fine; only overrides and
        // isolates are refused.
        assert!(parse("banner = \"\u{5E9}\u{5DC}\u{5D5}\u{5DD} \u{200F}!\"".as_bytes()).is_ok());
        assert!(parse(format!("banner = \"{}\"", "é".repeat(MAX_BANNER)).as_bytes()).is_ok(), "characters, not bytes");
    }

    #[test]
    fn refuses_invalid_input() {
        assert!(refused(&format!("banner = \"{}\"", "x".repeat(MAX_INPUT))).contains("16 KiB"));
        assert_eq!(parse(&[b'b', 0xff]), Err(invalid("not UTF-8")));
        assert!(refused("user-list = ").starts_with("not TOML"));
        assert!(refused("user-list = true\nuser-list = false").starts_with("not TOML"), "a key twice");
        // Unknown keys and tables.
        assert!(refused("theme = \"dark\"").contains("unknown key \"theme\""));
        assert!(refused("[banner]\ntext = \"x\"").contains("banner must be a string"));
        assert!(refused("[greeter]\nbanner = \"x\"").contains("unknown key"));
        assert!(refused("User-List = true").contains("unknown key"));
        // Types.
        assert_eq!(refused("user-list = \"false\""), "user-list must be true or false");
        assert_eq!(refused("user-list = 0"), "user-list must be true or false");
        assert_eq!(refused("banner = 1"), "banner must be a string");
        assert_eq!(refused("accent = [\"blue\"]"), "accent must be a string");
        assert_eq!(refused("clock-format = 24"), "clock-format must be a string");
        // Values.
        assert!(refused("accent = \"magenta\"").starts_with("accent must be one of blue, teal,"));
        assert!(refused("accent = \"Blue\"").starts_with("accent must be one of"));
        assert!(refused("accent = \" blue\"").starts_with("accent must be one of"));
        assert_eq!(refused("clock-format = \"12\""), "clock-format must be 12h or 24h");
        assert_eq!(refused("clock-format = \"24H\""), "clock-format must be 12h or 24h");
        // The banner.
        assert!(refused("banner = \"\"").contains("remove it"));
        assert!(refused("banner = \"  \"").contains("remove it"));
        assert!(refused(&format!("banner = \"{}\"", "x".repeat(MAX_BANNER + 1))).contains("longer than 200"));
        for c in ["\\n", "\\t", "\\r", "\\u0000", "\\u001b[31m", "\\u007f", "\\u0085"] {
            assert!(refused(&format!("banner = \"a{c}b\"")).contains("control character"), "{c}");
        }
        for c in ['\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}'] {
            assert!(refused(&format!("banner = \"a{c}b\"")).contains("bidirectional"), "{c:?}");
        }
        // Removing.
        assert_eq!(refused("remove = \"banner\""), "remove must be an array of key names");
        assert_eq!(refused("remove = [1]"), "remove must be an array of key names");
        assert!(refused("remove = [\"theme\"]").contains("unknown key \"theme\""));
        assert!(refused("remove = [\"remove\"]").contains("unknown key"));
        assert_eq!(refused("banner = \"x\"\nremove = [\"banner\"]"), "banner is both set and removed");
        // A long unknown key is shortened in the message.
        assert!(refused(&format!("{} = 1", "k".repeat(1000))).len() < 80);
    }

    #[test]
    fn keeps_what_it_does_not_change() {
        let existing = "# The login screen (written by hand).\n\
                        user-list = true   # show the users\n\
                        theme = \"dark\"\n\
                        banner = \"Old\"\n\
                        \n\
                        [future]\n\
                        option = 1\n";
        let changes = parse(b"user-list = false\naccent = \"red\"\nremove = [\"banner\", \"clock-format\"]").unwrap();
        let text = apply(existing, &changes, Path::new("greeter.toml")).unwrap();
        assert!(text.starts_with("# The login screen (written by hand).\nuser-list = false   # show the users\ntheme = \"dark\"\n"), "{text}");
        assert!(!text.contains("banner"), "{text}");
        assert!(text.contains("[future]\noption = 1\n"), "{text}");
        // The new key is a top-level key, not one of [future]'s.
        let parsed: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(parsed["accent"].as_str(), Some("red"));
        assert_eq!(parsed["theme"].as_str(), Some("dark"));
        assert_eq!(parsed["future"]["option"].as_integer(), Some(1));
        assert!(parsed["future"].get("accent").is_none());
        let config = Config::parse(&text).unwrap();
        assert!(!config.user_list);
        assert_eq!(config.accent.as_deref(), Some("red"));
        assert_eq!(config.banner, None);
    }

    #[test]
    fn writes_values_the_greeter_reads() {
        let banner = "Say \"hi\" \\ «bonjour» — 你好 \u{5E9}\u{5DC}\u{5D5}\u{5DD}";
        let input = format!("banner = {}\nclock-format = \"12h\"", toml::Value::String(banner.into()));
        let text = apply("", &parse(input.as_bytes()).unwrap(), Path::new("greeter.toml")).unwrap();
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.banner.as_deref(), Some(banner));
        assert_eq!(config.clock_format.as_deref(), Some("12h"));
        assert!(config.user_list, "unset: the default");
        // Removing from an empty file is nothing.
        assert_eq!(apply("", &parse(b"remove = [\"accent\"]").unwrap(), Path::new("greeter.toml")).unwrap(), "");
    }

    /// A scratch directory standing for `/etc`; the configuration goes in
    /// its `helix/greeter.toml`.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("helix-login-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn writes_atomically_with_mode_644() {
        let etc = scratch("write");
        let target = etc.join("helix/greeter.toml");
        // The directory is created.
        install(b"banner = \"Hello\"", &target).unwrap();
        assert_eq!(std::fs::metadata(etc.join("helix")).unwrap().permissions().mode() & 0o7777, 0o755);
        let meta = std::fs::metadata(&target).unwrap();
        assert_eq!(meta.permissions().mode() & 0o7777, 0o644);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "banner = \"Hello\"\n");
        // Replaced, not rewritten in place: a new inode.
        let inode = meta.ino();
        install(b"user-list = false", &target).unwrap();
        assert_ne!(std::fs::metadata(&target).unwrap().ino(), inode);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "banner = \"Hello\"\nuser-list = false\n");
        assert_eq!(entries(&etc.join("helix")), ["greeter.toml"], "no temporary file left");
        // Nothing to change: nothing written.
        let inode = std::fs::metadata(&target).unwrap().ino();
        install(b"", &target).unwrap();
        assert_eq!(std::fs::metadata(&target).unwrap().ino(), inode);
        // Invalid input: the file is not touched.
        assert!(matches!(install(b"accent = \"magenta\"", &target), Err(Error::Invalid(_))));
        assert_eq!(std::fs::metadata(&target).unwrap().ino(), inode);
        std::fs::remove_dir_all(&etc).unwrap();
    }

    #[test]
    fn leaves_a_file_it_cannot_read_alone() {
        let etc = scratch("broken");
        std::fs::create_dir(etc.join("helix")).unwrap();
        let target = etc.join("helix/greeter.toml");
        // Broken TOML: left as it is.
        std::fs::write(&target, "user-list = [\n# the admin's notes\n").unwrap();
        let err = install(b"user-list = false", &target).unwrap_err();
        assert!(matches!(&err, Error::Io(e) if e.contains("not valid TOML") && !e.contains('\n')), "{err:?}");
        assert_eq!(err.exit_code(), 2);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "user-list = [\n# the admin's notes\n");
        // Not UTF-8.
        std::fs::write(&target, b"banner = \"\xff\"\n").unwrap();
        assert!(matches!(install(b"user-list = false", &target), Err(Error::Io(e)) if e.contains("UTF-8")));
        // A symbolic link is not followed, and stays.
        let elsewhere = etc.join("elsewhere.toml");
        std::fs::write(&elsewhere, "theme = \"x\"\n").unwrap();
        std::fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &target).unwrap();
        assert!(matches!(install(b"user-list = false", &target), Err(Error::Io(e)) if e.contains("symbolic link")));
        assert!(std::fs::symlink_metadata(&target).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "theme = \"x\"\n");
        // A dangling link likewise (it must not create the file it names).
        std::fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink(etc.join("nowhere.toml"), &target).unwrap();
        assert!(matches!(install(b"user-list = false", &target), Err(Error::Io(_))));
        assert!(!etc.join("nowhere.toml").exists());
        // Not a regular file.
        std::fs::remove_file(&target).unwrap();
        std::fs::create_dir(&target).unwrap();
        assert!(matches!(install(b"user-list = false", &target), Err(Error::Io(e)) if e.contains("not a regular file")));
        // Too large.
        std::fs::remove_dir(&target).unwrap();
        std::fs::write(&target, format!("# {}\n", "x".repeat(MAX_FILE as usize))).unwrap();
        assert!(matches!(install(b"user-list = false", &target), Err(Error::Io(e)) if e.contains("1 MiB")));
        // The directory's parent must exist; nothing above it is created.
        let deep = etc.join("missing/helix/greeter.toml");
        assert!(matches!(install(b"user-list = false", &deep), Err(Error::Io(_))));
        assert!(!etc.join("missing").exists());
        std::fs::remove_dir_all(&etc).unwrap();
    }
}
