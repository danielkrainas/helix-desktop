//! Test helpers shared across Helix crates.
//!
//! - [`TestBus`]: a private `dbus-daemon` per test.
//! - [`logind`]: a fake systemd-logind.
//! - [`conformance`]: compare an exported D-Bus interface against the
//!   contract XML in helix-desktop-spec.

pub mod conformance;
pub mod logind;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use zbus::{connection, Connection};

/// A private `dbus-daemon` that is killed on drop.
pub struct TestBus {
    child: Child,
    dir: PathBuf,
    address: String,
}

impl TestBus {
    /// Start a bus, or return `None` (after printing a notice) if
    /// `dbus-daemon` is not installed. With `HELIX_REQUIRE_DBUS=1` (as in
    /// CI), a missing `dbus-daemon` is a failure instead of a skip.
    pub fn start() -> Option<Self> {
        Self::start_inner(None)
    }

    /// Like [`start`](Self::start), activating services from the `.service`
    /// files in `services` (and no others).
    pub fn start_with_services(services: &std::path::Path) -> Option<Self> {
        Self::start_inner(Some(services))
    }

    fn start_inner(services: Option<&std::path::Path>) -> Option<Self> {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "helix-test-bus-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create bus directory");
        let socket = dir.join("bus");
        let address = format!("unix:path={}", socket.display());

        let mut command = Command::new("dbus-daemon");
        command.args(["--nofork", "--nopidfile"]);
        match services {
            None => {
                command.arg("--session").arg(format!("--address={address}"));
            }
            Some(services) => {
                let config = dir.join("bus.conf");
                std::fs::write(
                    &config,
                    format!(
                        "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\"\n \
                         \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
                         <busconfig><type>session</type><listen>{address}</listen>\
                         <servicedir>{}</servicedir>\
                         <policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/>\
                         <allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>\n",
                        services.display()
                    ),
                )
                .expect("write bus config");
                command.arg(format!("--config-file={}", config.display()));
            }
        }
        let child = match command
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                assert!(
                    std::env::var_os("HELIX_REQUIRE_DBUS").is_none_or(|v| v != "1"),
                    "dbus-daemon not found and HELIX_REQUIRE_DBUS=1"
                );
                eprintln!("dbus-daemon not found; skipping D-Bus test");
                let _ = std::fs::remove_dir_all(&dir);
                return None;
            }
            Err(e) => panic!("failed to spawn dbus-daemon: {e}"),
        };

        let bus = Self {
            child,
            dir,
            address,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(
                Instant::now() < deadline,
                "dbus-daemon did not create its socket"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        Some(bus)
    }

    /// The bus address, e.g. for `DBUS_SESSION_BUS_ADDRESS`.
    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn builder(&self) -> connection::Builder<'static> {
        connection::Builder::address(self.address.as_str()).expect("valid bus address")
    }

    pub async fn connect(&self) -> Connection {
        self.builder().build().await.expect("connect to test bus")
    }
}

impl Drop for TestBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
