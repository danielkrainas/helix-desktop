//! The clipboard methods (helixdesktop/spec `contracts/agent-access/README.md`,
//! "Clipboard methods"; helixdesktop/desktop#151): `GetClipboard` and
//! `SetClipboard`, behind the `clipboard` switch.
//!
//! Agents never touch the clipboard themselves. The UI asks the compositor
//! what the clipboard offers and who set it (`helix_screenshot_v1` since
//! version 7), and has it write the selection into a pipe the agent code
//! reads; setting hands the compositor the data to own on the agent's
//! behalf. What is read is always the selection that was checked: the
//! compositor serves only the generation it reported.
//!
//! Reading is refused when the content came from the shell, Settings, a
//! system prompt's program or an excluded app, and when a password manager
//! marked it secret (`x-kde-passwordManagerHint`). The data itself is never
//! logged: only its type (a known kind) and length.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use zbus::zvariant::OwnedValue;

use super::policy::Target;
use super::{value, AgentError};

/// The most an agent reads or sets, in bytes.
pub const MAX_BYTES: usize = 16 << 20;
/// How long the app that copied has to send it.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// The type password managers (KeePassXC, KDE's) mark secrets with, its
/// value being `secret`.
pub const PASSWORD_HINT: &str = "x-kde-passwordManagerHint";
/// How long the hint may take to read.
const HINT_TIMEOUT: Duration = Duration::from_secs(1);
/// Plain text, in the order it is read.
pub const TEXT_TYPES: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];
/// What plain text is reported as.
pub const TEXT: &str = "text/plain;charset=utf-8";
/// The types an agent may set.
pub const SET_TYPES: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "text/html", "text/uri-list", "image/png"];

/// Who set the clipboard, as the compositor says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceKind {
    /// Nothing is on the clipboard.
    #[default]
    None,
    /// A Wayland app.
    App,
    /// An X11 app.
    X11,
    /// The shell: its text fields, search results and screenshots.
    Shell,
    /// An agent (`SetClipboard`).
    Agent,
    /// A remote desktop session's client.
    Remote,
}

impl SourceKind {
    /// The compositor's `helix_clipboard_info_v1.source`.
    pub fn from_wire(n: u32) -> SourceKind {
        match n {
            1 => SourceKind::App,
            2 => SourceKind::X11,
            3 => SourceKind::Shell,
            4 => SourceKind::Agent,
            5 => SourceKind::Remote,
            _ => SourceKind::None,
        }
    }
}

/// What the clipboard offers now, and who set it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Offer {
    pub types: Vec<String>,
    /// Changes whenever the clipboard does.
    pub generation: u32,
    pub source: SourceKind,
    /// The app id of a window of the app that set it ("": unknown).
    pub app_id: String,
    /// Its process (0: unknown).
    pub pid: u32,
}

/// A request for the UI.
#[derive(Debug)]
pub enum Request {
    /// What the clipboard offers.
    Peek(oneshot::Sender<Result<Offer, String>>),
    /// The selection of `generation` as `mime`: the pipe it comes through.
    Read { generation: u32, mime: String, reply: oneshot::Sender<Result<OwnedFd, String>> },
    /// Take the clipboard with `data` of type `mime`, for an agent.
    Set { mime: String, data: Vec<u8>, reply: oneshot::Sender<Result<(), String>> },
}

/// A [`Request`] that can travel in a `UiEvent`.
#[derive(Debug, Clone)]
pub struct ClipboardSlot(Arc<Mutex<Option<Request>>>);

impl ClipboardSlot {
    pub fn new(request: Request) -> Self {
        Self(Arc::new(Mutex::new(Some(request))))
    }

    pub fn take(&self) -> Option<Request> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl PartialEq for ClipboardSlot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

fn is_plain(mime: &str) -> bool {
    TEXT_TYPES.iter().any(|t| t.eq_ignore_ascii_case(mime))
}

/// Whether `mime` is given as `text` (when it is UTF-8).
pub fn is_text(mime: &str) -> bool {
    is_plain(mime) || mime.to_ascii_lowercase().starts_with("text/")
}

/// The type to read of `types`: `want` (`text/plain` or `text` for plain
/// text of any kind, `major/*` for the first of that kind, else exactly),
/// or by default plain text, else a picture (`image/png`), else the first
/// type offered.
pub fn pick(types: &[String], want: Option<&str>) -> Result<String, AgentError> {
    let offered = |t: &str| types.iter().find(|m| m.eq_ignore_ascii_case(t)).cloned();
    let plain = || TEXT_TYPES.iter().find_map(|t| offered(t));
    if types.is_empty() {
        return Err(AgentError::NotFound("the clipboard is empty".into()));
    }
    let missing = |w: &str| AgentError::NotFound(format!("the clipboard has no {w}"));
    match want {
        None => Ok(plain()
            .or_else(|| offered("image/png"))
            .or_else(|| types.iter().find(|t| t.contains('/') && !t.eq_ignore_ascii_case(PASSWORD_HINT)).cloned())
            .unwrap_or_else(|| types[0].clone())),
        Some(w) if w.eq_ignore_ascii_case("text") || is_plain(w) => plain().ok_or_else(|| missing(w)),
        Some(w) if w.ends_with("/*") => {
            let major = w[..w.len() - 1].to_ascii_lowercase();
            types.iter().find(|t| t.to_ascii_lowercase().starts_with(&major)).cloned().ok_or_else(|| missing(w))
        }
        Some(w) => offered(w).ok_or_else(|| missing(w)),
    }
}

/// The Clipboard record of `bytes` read as `mime` (README.md, "Records"):
/// `mime-type`, and `text` for text types that are text, else `data`;
/// with `types`, everything the clipboard offers.
pub fn record(mime: &str, bytes: Vec<u8>, types: &[String]) -> HashMap<String, OwnedValue> {
    let mut out = HashMap::new();
    out.insert("types".to_owned(), value(types.to_vec()));
    let reported = if is_plain(mime) { TEXT } else { mime };
    out.insert("mime-type".to_owned(), value(reported));
    if is_text(mime) {
        match String::from_utf8(bytes) {
            Ok(text) => {
                out.insert("text".to_owned(), value(text));
            }
            // X11's STRING is Latin-1.
            Err(e) if is_plain(mime) => {
                let text: String = e.into_bytes().into_iter().map(char::from).collect();
                out.insert("text".to_owned(), value(text));
            }
            Err(e) => {
                out.insert("data".to_owned(), value(e.into_bytes()));
            }
        }
    } else {
        out.insert("data".to_owned(), value(bytes));
    }
    out
}

/// A MIME type as the activity log records it: a common kind, else
/// `other` (apps name their own types).
pub fn log_kind(mime: &str) -> &'static str {
    if is_plain(mime) {
        return "text/plain";
    }
    const KNOWN: [&str; 8] = ["text/html", "text/uri-list", "image/png", "image/jpeg", "image/gif", "image/svg+xml", "application/pdf", PASSWORD_HINT];
    KNOWN.iter().find(|k| k.eq_ignore_ascii_case(mime)).copied().unwrap_or("other")
}

/// `SetClipboard`'s type and data checked: the type to offer.
pub fn check_set(mime: &str, data: &[u8]) -> Result<String, AgentError> {
    let mime = if mime.is_empty() { TEXT.to_owned() } else { mime.to_ascii_lowercase() };
    if !SET_TYPES.contains(&mime.as_str()) {
        return Err(AgentError::NotSupported(format!("agents can set {} only", SET_TYPES[1..].join(", "))));
    }
    if data.is_empty() {
        return Err(AgentError::invalid_args("nothing to put on the clipboard"));
    }
    if data.len() > MAX_BYTES {
        return Err(AgentError::invalid_args(format!("at most {} MiB", MAX_BYTES >> 20)));
    }
    if mime.starts_with("text/") && std::str::from_utf8(data).is_err() {
        return Err(AgentError::invalid_args("text must be UTF-8"));
    }
    if mime == "image/png" && !data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(AgentError::invalid_args("not a PNG image"));
    }
    Ok(if mime == "text/plain" { TEXT.to_owned() } else { mime })
}

/// Step 9 for the clipboard's source, as far as the compositor told it:
/// the shell's own content, or the app that set it (checked against the
/// exclusions); `None` for an agent's copy, a remote desktop session's
/// and an unknown app. The process is checked too
/// ([`super::a11y::check_process`]).
pub fn source_target(offer: &Offer) -> Option<Target<'_>> {
    match offer.source {
        SourceKind::Shell => Some(Target::Shell),
        SourceKind::App | SourceKind::X11 if !offer.app_id.is_empty() => Some(Target::App(&offer.app_id)),
        _ => None,
    }
}

/// A boolean option (absent: false).
pub fn flag(options: &HashMap<String, OwnedValue>, key: &str) -> Result<bool, AgentError> {
    match options.get(key) {
        None => Ok(false),
        Some(v) => bool::try_from(v.clone()).map_err(|_| AgentError::invalid_args(format!("“{key}” must be a boolean"))),
    }
}

/// Whether a password manager's hint (its bytes) says the content is a
/// secret.
pub fn hint_is_secret(hint: &[u8]) -> bool {
    String::from_utf8_lossy(hint).trim_matches(|c: char| c.is_whitespace() || c == '\0').eq_ignore_ascii_case("secret")
}

/// How long the hint may take.
pub fn hint_timeout() -> Duration {
    HINT_TIMEOUT
}

/// Why reading a pipe stopped short.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    TooLarge,
    TimedOut,
    Failed(String),
}

/// Read `fd` to its end: at most `max` bytes, until `timeout` (the app
/// that copied may never close it). Blocks.
pub fn read_pipe(fd: OwnedFd, max: usize, timeout: Duration) -> Result<Vec<u8>, ReadError> {
    let deadline = Instant::now() + timeout;
    let raw = fd.as_raw_fd();
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ReadError::TimedOut);
        }
        let mut pfd = libc::pollfd { fd: raw, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut pfd, 1, left.as_millis().clamp(1, i32::MAX as u128) as i32) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(ReadError::Failed(err.to_string()));
        }
        if ready == 0 {
            continue;
        }
        // SAFETY: reading into our own buffer.
        let n = unsafe { libc::read(raw, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) {
                continue;
            }
            return Err(ReadError::Failed(err.to_string()));
        }
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n as usize]);
        if out.len() > max {
            return Err(ReadError::TooLarge);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::policy::{self, Config};
    use std::io::Write;

    fn types(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn picks_a_type() {
        let firefox = types(&["text/html", "text/_moz_htmlcontext", "UTF8_STRING", "text/plain;charset=utf-8", "image/png"]);
        assert_eq!(pick(&firefox, None).unwrap(), "text/plain;charset=utf-8", "plain text first");
        assert_eq!(pick(&firefox, Some("text/plain")).unwrap(), "text/plain;charset=utf-8");
        assert_eq!(pick(&firefox, Some("text")).unwrap(), "text/plain;charset=utf-8");
        assert_eq!(pick(&firefox, Some("text/HTML")).unwrap(), "text/html", "ignoring case");
        assert_eq!(pick(&firefox, Some("image/*")).unwrap(), "image/png");
        assert_eq!(pick(&types(&["UTF8_STRING", "STRING"]), None).unwrap(), "UTF8_STRING");
        assert_eq!(pick(&types(&["image/jpeg", "image/png"]), None).unwrap(), "image/png");
        assert_eq!(pick(&types(&[PASSWORD_HINT, "application/x-thing"]), None).unwrap(), "application/x-thing");
        assert_eq!(pick(&firefox, Some("application/pdf")).unwrap_err().kind(), "NotFound");
        assert_eq!(pick(&types(&["image/png"]), Some("text/plain")).unwrap_err().kind(), "NotFound");
        assert_eq!(pick(&[], None).unwrap_err().kind(), "NotFound");
    }

    fn text_of(r: &HashMap<String, OwnedValue>) -> Option<String> {
        r.get("text").map(|v| String::try_from(v.clone()).unwrap())
    }

    #[test]
    fn records() {
        let offered = types(&["UTF8_STRING"]);
        let r = record("UTF8_STRING", "héllo".as_bytes().to_vec(), &offered);
        assert_eq!(String::try_from(r["mime-type"].clone()).unwrap(), TEXT, "plain text as one type");
        assert_eq!(text_of(&r).as_deref(), Some("héllo"));
        assert_eq!(<Vec<String>>::try_from(r["types"].clone()).unwrap(), offered);
        let r = record("STRING", vec![b'h', 0xE9], &offered);
        assert_eq!(text_of(&r).as_deref(), Some("hé"), "Latin-1");
        let r = record("text/uri-list", b"file:///tmp/a\r\n".to_vec(), &offered);
        assert_eq!(text_of(&r).as_deref(), Some("file:///tmp/a\r\n"));
        let r = record("text/html", vec![0xFF, 0xFE], &offered);
        assert!(r.contains_key("data") && !r.contains_key("text"), "not UTF-8: as bytes");
        let r = record("image/png", vec![1, 2, 3], &offered);
        assert_eq!(<Vec<u8>>::try_from(r["data"].clone()).unwrap(), [1, 2, 3]);
        assert!(!r.contains_key("text"));
    }

    #[test]
    fn log_kinds() {
        assert_eq!(log_kind("UTF8_STRING"), "text/plain");
        assert_eq!(log_kind("image/PNG"), "image/png");
        assert_eq!(log_kind("application/x-secret-thing"), "other", "apps' own types are not recorded");
    }

    #[test]
    fn checks_what_is_set() {
        assert_eq!(check_set("", b"hi").unwrap(), TEXT);
        assert_eq!(check_set("text/plain", b"hi").unwrap(), TEXT);
        assert_eq!(check_set("Text/HTML", b"<b>hi</b>").unwrap(), "text/html");
        assert_eq!(check_set("text/uri-list", b"file:///tmp/a").unwrap(), "text/uri-list");
        assert_eq!(check_set("image/png", b"\x89PNG\r\n\x1a\nrest").unwrap(), "image/png");
        assert_eq!(check_set("image/png", b"GIF89a").unwrap_err().kind(), "InvalidArgs");
        assert_eq!(check_set("x-special/gnome-copied-files", b"copy\nfile:///etc/passwd").unwrap_err().kind(), "NotSupported");
        assert_eq!(check_set(PASSWORD_HINT, b"secret").unwrap_err().kind(), "NotSupported");
        assert_eq!(check_set("", b"").unwrap_err().kind(), "InvalidArgs");
        assert_eq!(check_set("", &[0xFF]).unwrap_err().kind(), "InvalidArgs", "text must be UTF-8");
        assert_eq!(check_set("", &vec![b'a'; MAX_BYTES + 1]).unwrap_err().kind(), "InvalidArgs");
    }

    #[test]
    fn sources() {
        let config = Config { enabled: true, excluded_apps: vec!["org.keepassxc.KeePassXC".into()], ..Config::default() };
        let from = |source, app: &str| Offer { types: types(&[TEXT]), source, app_id: app.into(), ..Offer::default() };
        let token = |offer: Offer| match source_target(&offer).map_or(Ok(()), |t| policy::check_target(&config, t)) {
            Err(AgentError::Excluded(t)) => t,
            Ok(()) => "ok".into(),
            Err(other) => panic!("{other:?}"),
        };
        assert_eq!(token(from(SourceKind::App, "org.gnome.TextEditor")), "ok");
        assert_eq!(token(from(SourceKind::App, "org.keepassxc.KeePassXC")), "app:org.keepassxc.KeePassXC");
        assert_eq!(token(from(SourceKind::X11, "org.keepassxc.KeePassXC")), "app:org.keepassxc.KeePassXC");
        assert_eq!(token(from(SourceKind::App, "org.helixdesktop.Settings")), "settings");
        assert_eq!(token(from(SourceKind::Shell, "")), "shell");
        assert_eq!(token(from(SourceKind::App, "")), "ok", "unknown app: allowed");
        assert_eq!(token(from(SourceKind::Agent, "")), "ok");
        assert_eq!(token(from(SourceKind::Remote, "")), "ok");
        assert_eq!(SourceKind::from_wire(3), SourceKind::Shell);
        assert_eq!(SourceKind::from_wire(99), SourceKind::None);
    }

    #[test]
    fn password_hints() {
        assert!(hint_is_secret(b"secret") && hint_is_secret(b"secret\n") && hint_is_secret(b"secret\0"));
        assert!(!hint_is_secret(b"") && !hint_is_secret(b"public"));
    }

    #[test]
    fn reads_pipes() {
        let (r, mut w) = std::io::pipe().unwrap();
        w.write_all(b"hello").unwrap();
        drop(w);
        assert_eq!(read_pipe(r.into(), 100, Duration::from_secs(1)).unwrap(), b"hello");
        let (r, mut w) = std::io::pipe().unwrap();
        w.write_all(&[0; 200]).unwrap();
        drop(w);
        assert_eq!(read_pipe(r.into(), 100, Duration::from_secs(1)), Err(ReadError::TooLarge));
        // A writer that never closes.
        let (r, _w) = std::io::pipe().unwrap();
        let start = Instant::now();
        assert_eq!(read_pipe(r.into(), 100, Duration::from_millis(200)), Err(ReadError::TimedOut));
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
