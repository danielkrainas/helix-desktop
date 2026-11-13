//! Holding Shift at login turns extensions off for the session
//! (helix-desktop-spec `contracts/extensions/README.md`, "Enabling,
//! approving and placing"): the compositor reads the keyboards' held keys
//! when it first starts the shell, and tells the shell.

use std::os::fd::AsRawFd;

use super::Helix;

const KEY_LEFTSHIFT: usize = 42;
const KEY_RIGHTSHIFT: usize = 54;
/// Bytes of a key bitmap (`KEY_MAX` 0x2ff).
const KEY_BYTES: usize = 96;

/// Whether a key bitmap (`EVIOCGKEY`) has a Shift key down.
pub fn shift_held(bits: &[u8]) -> bool {
    [KEY_LEFTSHIFT, KEY_RIGHTSHIFT].iter().any(|k| bits.get(k / 8).is_some_and(|b| b & (1 << (k % 8)) != 0))
}

/// `EVIOCGKEY(len)`: _IOC(_IOC_READ, 'E', 0x18, len).
fn eviocgkey(len: usize) -> libc::c_ulong {
    ((2u64 << 30) | ((len as u64) << 16) | ((b'E' as u64) << 8) | 0x18) as libc::c_ulong
}

impl Helix {
    /// Whether a Shift key is held now, on any of the seat's input
    /// devices (DRM only; `HELIX_TEST_SHIFT=1` otherwise, for tests).
    pub fn shift_held_now(&mut self) -> bool {
        let Some(udev) = self.udev.as_mut() else {
            return std::env::var("HELIX_TEST_SHIFT").as_deref() == Ok("1");
        };
        use smithay::backend::session::Session;
        let Ok(entries) = std::fs::read_dir("/dev/input") else { return false };
        let mut nodes: Vec<std::path::PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("event"))).collect();
        nodes.sort();
        for node in nodes {
            let flags = smithay::reexports::rustix::fs::OFlags::RDONLY | smithay::reexports::rustix::fs::OFlags::NONBLOCK | smithay::reexports::rustix::fs::OFlags::CLOEXEC;
            let Ok(fd) = udev.session.open(&node, flags) else { continue };
            let mut bits = [0u8; KEY_BYTES];
            // SAFETY: the buffer is KEY_BYTES long, as the request says.
            let ok = unsafe { libc::ioctl(fd.as_raw_fd(), eviocgkey(KEY_BYTES), bits.as_mut_ptr()) } >= 0;
            let _ = udev.session.close(fd);
            if ok && shift_held(&bits) {
                tracing::info!(device = %node.display(), "Shift held at login: extensions are off for this session");
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmaps() {
        let mut bits = [0u8; KEY_BYTES];
        assert!(!shift_held(&bits));
        bits[KEY_RIGHTSHIFT / 8] |= 1 << (KEY_RIGHTSHIFT % 8);
        assert!(shift_held(&bits));
        assert_eq!(eviocgkey(96), 0x8060_4518);
    }
}
