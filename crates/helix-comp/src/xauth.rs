//! X authorization for Xwayland: a random MIT-MAGIC-COOKIE-1 in an
//! Xauthority file only the user can read (helixdesktop/spec
//! `contracts/wayland/README.md`, "X authorization"), so only processes
//! given the file can connect.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const COOKIE_NAME: &str = "MIT-MAGIC-COOKIE-1";
/// `FamilyLocal`: a local connection, with the host name as address.
const FAMILY_LOCAL: u16 = 256;
/// `FamilyWild`: any address.
const FAMILY_WILD: u16 = 65535;

/// The cookie of this compositor's Xwayland.
#[derive(Clone, PartialEq, Eq)]
pub struct Cookie(pub [u8; 16]);

impl std::fmt::Debug for Cookie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cookie(..)")
    }
}

impl Cookie {
    /// 16 random bytes from the kernel.
    pub fn generate() -> std::io::Result<Self> {
        let mut bytes = [0u8; 16];
        let mut filled = 0;
        while filled < bytes.len() {
            // SAFETY: the pointer and length describe the rest of `bytes`.
            let n = unsafe { libc::getrandom(bytes[filled..].as_mut_ptr().cast(), bytes.len() - filled, 0) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            filled += n as usize;
        }
        Ok(Self(bytes))
    }
}

fn field(out: &mut Vec<u8>, data: &[u8]) {
    let len = u16::try_from(data.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&data[..usize::from(len)]);
}

fn entry(out: &mut Vec<u8>, family: u16, address: &[u8], display: &str, cookie: &Cookie) {
    out.extend_from_slice(&family.to_be_bytes());
    field(out, address);
    field(out, display.as_bytes());
    field(out, COOKIE_NAME.as_bytes());
    field(out, &cookie.0);
}

/// An Xauthority file's contents: for display `display` on host `host`
/// (local connections), and for it under any address (as mutter writes
/// it, so clients that look up a wildcard entry find it too). Without a
/// display number (not chosen yet) only the wildcard entry, for any
/// display: the server reads the cookie from it, whatever the entry says.
pub fn file_contents(host: &[u8], display: Option<u32>, cookie: &Cookie) -> Vec<u8> {
    let mut out = Vec::new();
    match display {
        Some(n) => {
            let n = n.to_string();
            entry(&mut out, FAMILY_LOCAL, host, &n, cookie);
            entry(&mut out, FAMILY_WILD, b"", &n, cookie);
        }
        None => entry(&mut out, FAMILY_WILD, b"", "", cookie),
    }
    out
}

/// This machine's host name.
pub fn host_name() -> Vec<u8> {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is writable for its length.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return Vec::new();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    buf[..end].to_vec()
}

/// Write `contents` to `path`, readable by the user only, replacing the
/// file at once (a reader sees the old or the new one).
pub fn write_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&tmp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// `$XDG_RUNTIME_DIR/.helix-Xwaylandauth.<pid>` (none without a runtime
/// directory).
pub fn default_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty())?;
    Some(Path::new(&runtime).join(format!(".helix-Xwaylandauth.{}", std::process::id())))
}

/// The running Xwayland's authorization: its file and cookie.
#[derive(Debug, Clone)]
pub struct Auth {
    pub path: PathBuf,
    pub cookie: Cookie,
}

static CURRENT: Mutex<Option<Auth>> = Mutex::new(None);

/// Remember the running Xwayland's authorization (for [`connect`]).
pub fn set_current(auth: Option<Auth>) {
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = auth;
}

pub fn current() -> Option<Auth> {
    CURRENT.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// A connection of the compositor's own to Xwayland's display `display`,
/// with the cookie. Returns the connection and its default screen.
pub fn connect(display: u32) -> Result<(x11rb::rust_connection::RustConnection, usize), String> {
    use x11rb::rust_connection::{DefaultStream, RustConnection};
    let Some(auth) = current() else {
        return RustConnection::connect(Some(&format!(":{display}"))).map_err(|e| e.to_string());
    };
    let socket = format!("/tmp/.X11-unix/X{display}");
    let stream = std::os::unix::net::UnixStream::connect(&socket).map_err(|e| format!("{socket}: {e}"))?;
    let (stream, _) = DefaultStream::from_unix_stream(stream).map_err(|e| e.to_string())?;
    let conn = RustConnection::connect_to_stream_with_auth_info(stream, 0, COOKIE_NAME.as_bytes().to_vec(), auth.cookie.0.to_vec()).map_err(|e| e.to_string())?;
    Ok((conn, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xauthority_format() {
        let cookie = Cookie([7; 16]);
        let bytes = file_contents(b"box", Some(3), &cookie);
        let mut expected = vec![1, 0, 0, 3, b'b', b'o', b'x', 0, 1, b'3', 0, 18];
        expected.extend_from_slice(COOKIE_NAME.as_bytes());
        expected.extend_from_slice(&[0, 16]);
        expected.extend_from_slice(&[7; 16]);
        expected.extend_from_slice(&[0xff, 0xff, 0, 0, 0, 1, b'3', 0, 18]);
        expected.extend_from_slice(COOKIE_NAME.as_bytes());
        expected.extend_from_slice(&[0, 16]);
        expected.extend_from_slice(&[7; 16]);
        assert_eq!(bytes, expected);
        let any = file_contents(b"box", None, &cookie);
        assert_eq!(&any[..6], &[0xff, 0xff, 0, 0, 0, 0]);
    }

    #[test]
    fn cookies_are_random_and_files_private() {
        use std::os::unix::fs::PermissionsExt;
        let (a, b) = (Cookie::generate().unwrap(), Cookie::generate().unwrap());
        assert_ne!(a, b);
        let dir = std::env::temp_dir().join(format!("helix-xauth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth");
        write_file(&path, &file_contents(b"h", None, &a)).unwrap();
        write_file(&path, &file_contents(b"h", Some(1), &a)).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), file_contents(b"h", Some(1), &a));
        assert_eq!(format!("{a:?}"), "Cookie(..)", "never logged");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
