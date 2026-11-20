//! A private accessibility bus and a GTK 4 window to read over it, for
//! tests of Agent Access's *Control apps* methods: a `dbus-daemon` that
//! starts at-spi2-core's registry on its own (never the user's
//! accessibility bus), and `data/gtk4-controls.py`, a window of common
//! controls (a button, a text field, a password field, a field with
//! hidden characters, a slider) that prints what is done to it.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::TestBus;

/// The GTK 4 test window's program (Python with PyGObject).
pub const GTK4_CONTROLS: &str = include_str!("../data/gtk4-controls.py");
/// A GTK 3 test window's (AT-SPI through ATK's bridge).
pub const GTK3_CONTROLS: &str = include_str!("../data/gtk3-controls.py");

/// at-spi2-core's registry daemon, where it is installed.
pub fn registryd() -> Option<PathBuf> {
    ["/usr/libexec/at-spi2-registryd", "/usr/lib/at-spi2-core/at-spi2-registryd", "/usr/lib/at-spi2-registryd"].iter().map(PathBuf::from).find(|p| p.exists())
}

/// Whether `/usr/bin/python3` can show GTK 4 windows.
pub fn gtk4_python() -> bool {
    gtk_python("4.0")
}

/// Whether `/usr/bin/python3` can show GTK 3 windows.
pub fn gtk3_python() -> bool {
    gtk_python("3.0")
}

fn gtk_python(version: &str) -> bool {
    Command::new("/usr/bin/python3")
        .args(["-c", &format!("import gi; gi.require_version('Gtk', '{version}'); from gi.repository import Gtk")])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A private accessibility bus with at-spi2-core's registry, which it
/// starts when first asked for. Dropping it ends both (the registry does
/// not end with its bus by itself).
pub struct A11yBus {
    bus: TestBus,
    services: PathBuf,
}

impl A11yBus {
    /// Start the bus, or `None` (after a notice) without dbus-daemon or
    /// the registry.
    pub fn start() -> Option<A11yBus> {
        let Some(registry) = registryd() else {
            eprintln!("at-spi2-registryd is not installed; skipping");
            return None;
        };
        // Its own directory, so the registry's process can be found when
        // the bus is dropped whatever the test removed.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let services = std::env::temp_dir().join(format!("helix-test-a11y-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::create_dir_all(&services).ok()?;
        // The registry finds its bus in AT_SPI_BUS_ADDRESS (else it would
        // ask the session bus), and writes down its process to be ended.
        let pidfile = services.join("registryd.pid");
        let exec = format!("/bin/sh -c 'echo $$ > \"{}\"; AT_SPI_BUS_ADDRESS=\"$DBUS_STARTER_ADDRESS\" exec \"{}\"'", pidfile.display(), registry.display());
        std::fs::write(services.join("org.a11y.atspi.Registry.service"), format!("[D-BUS Service]\nName=org.a11y.atspi.Registry\nExec={exec}\n")).ok()?;
        let bus = TestBus::start_with_services(&services)?;
        Some(A11yBus { bus, services })
    }

    pub fn address(&self) -> &str {
        self.bus.address()
    }

    pub async fn connect(&self) -> zbus::Connection {
        self.bus.connect().await
    }
}

impl Drop for A11yBus {
    fn drop(&mut self) {
        let pidfile = self.services.join("registryd.pid");
        let pid = std::fs::read_to_string(&pidfile).ok().and_then(|p| p.trim().parse::<i32>().ok());
        // Only the registry this bus started.
        let ours = pid.filter(|p| std::fs::read_link(format!("/proc/{p}/exe")).is_ok_and(|e| e.to_string_lossy().contains("at-spi2-registryd")));
        if let Some(pid) = ours {
            // SAFETY: signalling a process this bus started.
            unsafe { kill(pid, 15) };
        }
        let _ = std::fs::remove_dir_all(&self.services);
    }
}

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// Write the GTK 4 test window's program to `dir`; its path.
pub fn write_gtk4_controls(dir: &Path) -> PathBuf {
    let path = dir.join("gtk4-controls.py");
    std::fs::write(&path, GTK4_CONTROLS).expect("write the test window's program");
    path
}

/// Write the GTK 3 test window's program to `dir`; its path.
pub fn write_gtk3_controls(dir: &Path) -> PathBuf {
    let path = dir.join("gtk3-controls.py");
    std::fs::write(&path, GTK3_CONTROLS).expect("write the test window's program");
    path
}
