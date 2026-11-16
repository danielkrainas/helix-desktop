//! The screen reader on the login screen (helixdesktop/spec
//! `components/greeter.md`, "Accessibility"): Orca, started by the
//! greeter as its own child with the greeter's environment (so on the
//! greeter's session bus and accessibility bus), and stopped by its own
//! process id. Never `orca --replace`: that would end any other Orca.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// The screen reader's program.
pub const PROGRAM: &str = "orca";

/// `name` looked up in `path` (a `PATH` value): the first executable file.
pub fn find(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?).filter(|dir| dir.is_absolute()).map(|dir| dir.join(name)).find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The running screen reader, if the greeter started one.
#[derive(Default)]
pub struct ScreenReader {
    child: Option<Child>,
}

impl ScreenReader {
    /// Start or stop it.
    pub fn set(&mut self, on: bool) {
        // Gone by itself (it quit, or crashed): forget it.
        if self.child.as_mut().is_some_and(|c| !matches!(c.try_wait(), Ok(None))) {
            self.child = None;
        }
        match (on, self.child.is_some()) {
            (true, false) => self.start(),
            (false, true) => self.stop(),
            _ => {}
        }
    }

    fn start(&mut self) {
        let Some(program) = find(PROGRAM, std::env::var_os("PATH").as_deref()) else {
            tracing::warn!("the screen reader (orca) is not installed");
            return;
        };
        match Command::new(&program).spawn() {
            Ok(child) => {
                tracing::info!(pid = child.id(), "screen reader started");
                self.child = Some(child);
            }
            Err(err) => tracing::warn!(%err, program = %program.display(), "cannot start the screen reader"),
        }
    }

    /// Ask it to quit (SIGTERM to its own pid); if it has not within two
    /// seconds, kill it. Reaped on a thread of its own.
    fn stop(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        let pid = child.id();
        tracing::info!(pid, "stopping the screen reader");
        if let Ok(pid) = libc::pid_t::try_from(pid) {
            // SAFETY: kill(2) on our own child's pid; the child is not
            // reaped yet (we hold it), so the pid cannot have been reused.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        let _ = std::thread::Builder::new().name("screen-reader-exit".into()).spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if !matches!(child.try_wait(), Ok(None)) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_programs_on_the_path() {
        let dir = std::env::temp_dir().join(format!("helix-greeter-path-{}", std::process::id()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        // Not executable in a, executable in b.
        std::fs::write(a.join("orca"), "").unwrap();
        std::fs::write(b.join("orca"), "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(b.join("orca"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([&a, &b]).unwrap();
        assert_eq!(find("orca", Some(&path)), Some(b.join("orca")));
        assert_eq!(find("orca", Some(OsStr::new("relative:"))), None, "relative directories are skipped");
        assert_eq!(find("orca", None), None);
        assert_eq!(find("not-there", Some(&path)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
