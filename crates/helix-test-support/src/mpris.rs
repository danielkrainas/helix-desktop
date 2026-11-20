//! Fake MPRIS media players for tests: each one a process of its own
//! (`data/fake-mpris.py`, Python with PyGObject) on a private bus, so the
//! shell sees a player's own process and bus name, never the user's
//! players.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The fake player's program.
pub const FAKE_MPRIS: &str = include_str!("../data/fake-mpris.py");

/// Whether `/usr/bin/python3` has PyGObject's Gio.
pub fn python_gio() -> bool {
    Command::new("/usr/bin/python3")
        .args(["-c", "import gi; gi.require_version('Gio', '2.0'); from gi.repository import Gio"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A running fake player; killed when dropped.
pub struct FakePlayer {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
}

impl FakePlayer {
    /// Start a player on the bus at `address`, as `config` (JSON, see
    /// `data/fake-mpris.py`) describes it, and wait until it owns its name.
    pub fn start(address: &str, config: &str) -> FakePlayer {
        let mut child = Command::new("/usr/bin/python3")
            .args(["-c", FAKE_MPRIS, config])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("DBUS_SESSION_BUS_ADDRESS", address)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start the fake media player");
        let lines = Arc::new(Mutex::new(Vec::new()));
        let out = child.stdout.take().expect("its output");
        let seen = lines.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                seen.lock().unwrap().push(line);
            }
        });
        let player = FakePlayer { child, lines };
        assert!(player.wait_for("ready", Duration::from_secs(10)), "the fake media player did not start: {config}");
        player
    }

    /// Its process id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The calls it got (`Seek -10500000`), in order.
    pub fn calls(&self) -> Vec<String> {
        self.lines.lock().unwrap().iter().filter_map(|l| l.strip_prefix("call ").map(str::to_owned)).collect()
    }

    /// Wait until it printed `line`.
    pub fn wait_for(&self, line: &str, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        while Instant::now() < until {
            if self.lines.lock().unwrap().iter().any(|l| l == line) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Wait until it got the call `call` (`PlayPause`).
    pub fn wait_call(&self, call: &str, timeout: Duration) -> bool {
        self.wait_for(&format!("call {call}"), timeout)
    }
}

impl Drop for FakePlayer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
