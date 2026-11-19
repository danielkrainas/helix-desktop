//! Wayland client sockets: who is on the other end, how far behind it is
//! reading, a roomier socket for the shell, pointer motion coalesced for a
//! shell that is behind, and what to log when a client goes away.
//!
//! The Wayland server library drops a client as soon as a message does not
//! fit in its outgoing buffer while the kernel's socket buffer is full too.
//! The shell gets a much larger buffer of both kinds, and while it is behind
//! it gets the pointer's latest position a few times a frame instead of every
//! motion of a 1000 Hz mouse, so a shell that stalls for a while catches up
//! instead of being dropped.

use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::Path;
use std::time::Duration;

pub use smithay::reexports::wayland_server::backend::DisconnectReason;

/// The kernel send buffer asked for on the shell's socket (the kernel caps
/// it at `net.core.wmem_max`).
pub const SHELL_SEND_BUFFER: usize = 4 << 20;

/// The most the server library may buffer for the shell when the socket is
/// full (it grows to this only while the shell is not reading).
pub const SHELL_MAX_BUFFER: usize = 16 << 20;

/// Unread bytes past which the shell counts as behind.
pub const BEHIND: usize = 16 << 10;

/// How often a shell that is behind gets the pointer's position.
pub const COALESCED_MOTION: Duration = Duration::from_millis(8);

/// Ask for a send buffer of `bytes` on `fd`; returns what the kernel
/// granted (Linux reports twice the usable size).
pub fn enlarge_send_buffer(fd: BorrowedFd<'_>, bytes: usize) -> std::io::Result<usize> {
    let want = libc::c_int::try_from(bytes).unwrap_or(libc::c_int::MAX);
    // SAFETY: a valid fd and a c_int option value of the right size.
    let set = unsafe {
        libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF, (&want as *const libc::c_int).cast(), std::mem::size_of::<libc::c_int>() as libc::socklen_t)
    };
    if set < 0 {
        return Err(std::io::Error::last_os_error());
    }
    send_buffer(fd)
}

/// The send buffer size of `fd`.
pub fn send_buffer(fd: BorrowedFd<'_>) -> std::io::Result<usize> {
    let mut got: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: a valid fd; `got` and `len` describe a c_int.
    let read = unsafe { libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF, (&mut got as *mut libc::c_int).cast(), &mut len) };
    if read < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(usize::try_from(got).unwrap_or(0))
}

/// Bytes written to the socket `fd` that its peer has not read yet, as the
/// kernel counts them (with its per-packet bookkeeping).
pub fn unread_bytes(fd: BorrowedFd<'_>) -> Option<usize> {
    let mut queued: libc::c_int = 0;
    // SAFETY: SIOCOUTQ writes one c_int.
    let ok = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCOUTQ, &mut queued as *mut libc::c_int) };
    (ok >= 0).then(|| usize::try_from(queued).unwrap_or(0))
}

/// The process on the other end of the Unix socket `fd`.
pub fn peer_pid(fd: BorrowedFd<'_>) -> Option<u32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: SO_PEERCRED fills a ucred of `len` bytes.
    let ok = unsafe { libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len) };
    (ok >= 0 && cred.pid > 0).then_some(cred.pid as u32)
}

/// Whether process `pid` runs in an app sandbox: a Flatpak
/// (`/.flatpak-info`), a snap (an AppArmor label starting with `snap.`),
/// or any other process whose root directory is not ours (or can't be
/// looked at).
pub fn sandboxed(pid: u32) -> bool {
    let label = std::fs::read_to_string(format!("/proc/{pid}/attr/apparmor/current")).or_else(|_| std::fs::read_to_string(format!("/proc/{pid}/attr/current"))).ok();
    sandboxed_process(label.as_deref(), Path::new(&format!("/proc/{pid}/root")), Path::new("/"))
}

/// [`sandboxed`] for a process with security label `label` and root
/// directory `root` (as `/proc/PID/root`), when ours is `own_root`.
pub fn sandboxed_process(label: Option<&str>, root: &Path, own_root: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if label.is_some_and(|l| l.trim_end_matches(['\0', '\n']).starts_with("snap.")) {
        return true;
    }
    if root.join(".flatpak-info").symlink_metadata().is_ok() {
        return true;
    }
    match (std::fs::metadata(root), std::fs::metadata(own_root)) {
        (Ok(theirs), Ok(ours)) => (theirs.dev(), theirs.ino()) != (ours.dev(), ours.ino()),
        _ => true,
    }
}

/// Process `pid`'s command name (`/proc/<pid>/comm`).
pub fn command_of(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).map(|c| c.trim_end().to_owned()).unwrap_or_default()
}

/// Why a client's connection ended, for the log. `unread`: bytes still
/// queued for it when it went (a client that exited leaves none; one that
/// stopped reading leaves its socket full).
pub fn describe_disconnect(reason: &DisconnectReason, unread: Option<usize>) -> String {
    match reason {
        DisconnectReason::ProtocolError(err) => {
            format!("protocol error {} on {}@{}: {}", err.code, err.object_interface, err.object_id, err.message)
        }
        DisconnectReason::ConnectionClosed => match unread {
            Some(n) if n > 0 => format!("connection closed with {n} bytes it had not read (it stopped reading its socket)"),
            _ => "connection closed".to_owned(),
        },
    }
}

/// One pointer motion from a device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Motion {
    /// Moved by a delta (accelerated and not), at `utime` (µs) / `time` (ms).
    Relative { delta: (f64, f64), unaccel: (f64, f64), utime: u64, time: u32 },
    /// Put at a position (a nested compositor's host pointer).
    Absolute { position: (f64, f64), utime: u64, time: u32 },
}

/// Pointer motion held back while the shell is behind: deltas add up,
/// positions replace each other, so what is sent later is where the pointer
/// is now.
#[derive(Debug, Default)]
pub struct MotionCoalescer {
    pending: Option<Motion>,
    /// Motions folded into the pending one (for the log).
    folded: usize,
    /// Whether the client under the pointer was behind at the last
    /// motion (for logging when that changes).
    pub behind: bool,
}

impl MotionCoalescer {
    /// Hold `motion`. A motion of another kind cannot be folded in: the
    /// held one comes back to be applied first.
    pub fn push(&mut self, motion: Motion) -> Option<Motion> {
        let merged = match (self.pending, motion) {
            (None, m) => Ok(m),
            (Some(Motion::Relative { delta: d0, unaccel: u0, .. }), Motion::Relative { delta, unaccel, utime, time }) => {
                Ok(Motion::Relative { delta: (d0.0 + delta.0, d0.1 + delta.1), unaccel: (u0.0 + unaccel.0, u0.1 + unaccel.1), utime, time })
            }
            (Some(Motion::Absolute { .. }), m @ Motion::Absolute { .. }) => Ok(m),
            (Some(old), m) => Err((old, m)),
        };
        match merged {
            Ok(m) => {
                if self.pending.is_some() {
                    self.folded += 1;
                }
                self.pending = Some(m);
                None
            }
            Err((old, m)) => {
                self.pending = Some(m);
                self.folded = 0;
                Some(old)
            }
        }
    }

    /// The motion held, if any, and how many it stands for.
    pub fn take(&mut self) -> Option<(Motion, usize)> {
        let folded = std::mem::take(&mut self.folded);
        self.pending.take().map(|m| (m, folded + 1))
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_none()
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn sandboxes() {
        use super::{sandboxed, sandboxed_process};
        let dir = std::env::temp_dir().join(format!("helix-client-sandbox-{}", std::process::id()));
        let (host, flatpak, other) = (dir.join("host"), dir.join("flatpak"), dir.join("other"));
        for d in [&host, &flatpak, &other] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(flatpak.join(".flatpak-info"), "[Application]\nname=org.example.App\n").unwrap();
        assert!(!sandboxed_process(None, &host, &host));
        assert!(!sandboxed_process(Some("unconfined\n"), &host, &host));
        assert!(sandboxed_process(Some("snap.firefox.firefox (enforce)\n"), &host, &host));
        assert!(sandboxed_process(None, &flatpak, &flatpak));
        assert!(sandboxed_process(None, &other, &host));
        assert!(sandboxed_process(None, &dir.join("gone"), &host));
        assert!(!sandboxed(std::process::id()), "this test");
        let _ = std::fs::remove_dir_all(&dir);
    }
    use std::io::Write;

    use super::*;

    #[test]
    fn deltas_add_up_and_positions_replace() {
        let mut c = MotionCoalescer::default();
        assert!(c.is_empty());
        assert_eq!(c.push(Motion::Relative { delta: (1.0, 2.0), unaccel: (0.5, 1.0), utime: 1000, time: 1 }), None);
        assert_eq!(c.push(Motion::Relative { delta: (3.0, -1.0), unaccel: (1.5, -0.5), utime: 2000, time: 2 }), None);
        assert_eq!(c.take(), Some((Motion::Relative { delta: (4.0, 1.0), unaccel: (2.0, 0.5), utime: 2000, time: 2 }, 2)));
        assert_eq!(c.take(), None);
        c.push(Motion::Absolute { position: (10.0, 10.0), utime: 1, time: 1 });
        c.push(Motion::Absolute { position: (20.0, 5.0), utime: 2, time: 2 });
        c.push(Motion::Absolute { position: (30.0, 7.0), utime: 3, time: 3 });
        assert_eq!(c.take(), Some((Motion::Absolute { position: (30.0, 7.0), utime: 3, time: 3 }, 3)));
    }

    #[test]
    fn a_motion_of_another_kind_flushes_the_held_one() {
        let mut c = MotionCoalescer::default();
        let rel = Motion::Relative { delta: (1.0, 0.0), unaccel: (1.0, 0.0), utime: 1, time: 1 };
        let abs = Motion::Absolute { position: (5.0, 5.0), utime: 2, time: 2 };
        c.push(rel);
        assert_eq!(c.push(abs), Some(rel));
        assert_eq!(c.take(), Some((abs, 1)));
    }

    #[test]
    fn socket_probes() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let before = send_buffer(ours.as_fd()).unwrap();
        let granted = enlarge_send_buffer(ours.as_fd(), SHELL_SEND_BUFFER).unwrap();
        assert!(granted >= before, "the kernel never shrinks it: {before} -> {granted}");
        assert_eq!(peer_pid(ours.as_fd()), Some(std::process::id()), "a socket pair's peer is this process");
        assert_eq!(command_of(std::process::id()), std::fs::read_to_string("/proc/self/comm").unwrap().trim_end());
        assert_eq!(unread_bytes(theirs.as_fd()), Some(0));
        theirs.write_all(&[0u8; 1000]).unwrap();
        // Counted as the kernel holds it (with its bookkeeping).
        assert!(unread_bytes(theirs.as_fd()).is_some_and(|n| n >= 1000), "written, not read by the peer");
        drop(ours);
        assert_eq!(unread_bytes(theirs.as_fd()), Some(0), "a peer that closed leaves nothing queued");
    }

    #[test]
    fn disconnect_reasons() {
        let closed = DisconnectReason::ConnectionClosed;
        assert_eq!(describe_disconnect(&closed, Some(0)), "connection closed");
        assert_eq!(describe_disconnect(&closed, None), "connection closed");
        assert!(describe_disconnect(&closed, Some(212_992)).contains("212992 bytes it had not read"));
        let error = DisconnectReason::ProtocolError(smithay::reexports::wayland_server::backend::protocol::ProtocolError {
            code: 2,
            object_id: 7,
            object_interface: "wl_surface".into(),
            message: "bad buffer".into(),
        });
        assert_eq!(describe_disconnect(&error, None), "protocol error 2 on wl_surface@7: bad buffer");
    }
}
