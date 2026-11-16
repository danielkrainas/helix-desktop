//! A whole headless Helix session for end-to-end tests: `helix-comp
//! --backend headless` running `helix-shell`, in a scratch home (every XDG
//! directory, the runtime directory and `TMPDIR` inside it), on a private
//! session bus with no activatable services, no system bus, no extensions
//! and no agents. Keys and pointer events go in through the compositor's
//! test input socket (`HELIX_HEADLESS_INPUT`, headless backend only), and
//! what the monitor shows comes out as a PNG snapshot every second.
//!
//! Nothing here reaches the user's session: the buses, sockets and
//! settings are the session's own and go away with it.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::TestBus;

/// Where `name` (a workspace binary) was built, next to the running test
/// binary (`target/<profile>/deps/…` → `target/<profile>/name`).
pub fn sibling_binary(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.parent()?;
    Some(dir.join(name)).filter(|p| p.exists())
}

/// A running headless session; ends (killing what it started) on drop.
pub struct HeadlessSession {
    comp: Child,
    bus: TestBus,
    home: PathBuf,
    input: BufReader<UnixStream>,
    /// `WAYLAND_DISPLAY` and `DISPLAY` as the compositor gave them to its
    /// command.
    pub wayland_display: PathBuf,
    pub x_display: String,
}

/// What to start.
pub struct Options<'a> {
    pub comp: &'a Path,
    pub shell: Option<&'a Path>,
    /// `WIDTHxHEIGHT` of the virtual monitor.
    pub size: (u32, u32),
    /// `GSETTINGS_BACKEND=keyfile` contents (`[org/gnome/…]` groups).
    pub settings: &'a str,
    /// More environment for the compositor and shell.
    pub env: Vec<(String, String)>,
}

impl HeadlessSession {
    /// Start the session, or `None` (after a notice) without
    /// `dbus-daemon` or Xwayland.
    pub fn start(options: Options<'_>) -> Option<Self> {
        Self::start_prepared(options, |_| {})
    }

    /// [`Self::start`], with `prepare` filling the scratch home first
    /// (files the session reads when it starts, such as
    /// `.config/user-dirs.dirs`).
    pub fn start_prepared(options: Options<'_>, prepare: impl FnOnce(&Path)) -> Option<Self> {
        let home = std::env::temp_dir().join(format!("helix-headless-{}-{}", std::process::id(), unique()));
        // No activatable services: nothing starts on the bus by itself.
        let bus_services = home.join("services");
        std::fs::create_dir_all(&bus_services).ok()?;
        let bus = TestBus::start_with_services(&bus_services)?;
        for dir in [".config/glib-2.0/settings", ".local/share", ".cache", ".local/state", "run", "tmp"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        std::fs::set_permissions(home.join("run"), std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        std::fs::write(home.join(".config/glib-2.0/settings/keyfile"), options.settings).unwrap();
        prepare(&home);
        let input_path = home.join("run/input");
        let announce = home.join("announce.sh");
        let displays = home.join("displays");
        std::fs::write(&announce, format!("#!/bin/sh\nprintf '%s\\n%s\\n' \"$WAYLAND_DISPLAY\" \"$DISPLAY\" > {}\n", displays.display())).unwrap();
        std::fs::set_permissions(&announce, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let mut cmd = Command::new(options.comp);
        cmd.args(["--backend", "headless", "--command"]).arg(&announce);
        if let Some(shell) = options.shell {
            cmd.arg("--shell").arg(shell);
        }
        cmd.env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_RUNTIME_DIR", home.join("run"))
            .env("TMPDIR", home.join("tmp"))
            .env("LANG", "C.UTF-8")
            .env("XDG_CURRENT_DESKTOP", "Helix")
            .env("GSETTINGS_BACKEND", "keyfile")
            .env("DBUS_SESSION_BUS_ADDRESS", bus.address())
            .env("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent/helix-test")
            .env("NO_AT_BRIDGE", "1")
            .env("HELIX_EXTENSIONS_OFF", "1")
            .env("HELIX_NO_POLKIT_AGENT", "1")
            .env("HELIX_NO_GEOCLUE_AGENT", "1")
            .env("HELIX_NO_NETWORK_AGENT", "1")
            .env("HELIX_NO_BLUETOOTH_AGENT", "1")
            .env("HELIX_NO_OBEX", "1")
            .env("HELIX_HEADLESS_SIZE", format!("{}x{}", options.size.0, options.size.1))
            .env("HELIX_HEADLESS_SNAPSHOT", home.join("snapshot.png"))
            .env("HELIX_HEADLESS_INPUT", &input_path)
            .env("RUST_LOG", std::env::var("HELIX_TEST_LOG").unwrap_or_else(|_| "info".into()))
            .envs(options.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(home.join("session.log")).map(Stdio::from).unwrap_or(Stdio::null()));
        let comp = cmd.spawn().expect("start helix-comp");
        let deadline = Instant::now() + Duration::from_secs(30);
        let (wayland, x) = loop {
            let lines = std::fs::read_to_string(&displays).unwrap_or_default();
            let mut lines = lines.lines();
            if let (Some(w), Some(x)) = (lines.next(), lines.next()) {
                if !w.is_empty() {
                    break (w.to_owned(), x.to_owned());
                }
            }
            assert!(Instant::now() < deadline, "the headless compositor did not start (log: {})", home.join("session.log").display());
            std::thread::sleep(Duration::from_millis(50));
        };
        let stream = loop {
            if let Ok(s) = UnixStream::connect(&input_path) {
                break s;
            }
            assert!(Instant::now() < deadline, "no test input socket");
            std::thread::sleep(Duration::from_millis(50));
        };
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let wayland_display = if Path::new(&wayland).is_absolute() { PathBuf::from(&wayland) } else { home.join("run").join(&wayland) };
        Some(Self { comp, bus, home, input: BufReader::new(stream), wayland_display, x_display: x })
    }

    /// The session's private bus.
    pub fn bus(&self) -> &TestBus {
        &self.bus
    }

    /// The scratch home.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The compositor's and shell's log so far (without color codes).
    pub fn log(&self) -> String {
        let raw = std::fs::read_to_string(self.home.join("session.log")).unwrap_or_default();
        let mut out = String::with_capacity(raw.len());
        let mut chars = raw.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // CSI … final byte.
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// Wait until the log has a line containing `text`.
    pub fn wait_for_log(&self, text: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.log().contains(text) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// Send test input commands (see helix-comp's `headless_input`), one
    /// per line, each answered before the next is sent.
    pub fn input(&mut self, commands: &str) {
        for line in commands.lines().map(str::trim).filter(|l| !l.is_empty()) {
            self.input.get_mut().write_all(format!("{line}\n").as_bytes()).expect("send test input");
            let mut answer = String::new();
            self.input.read_line(&mut answer).expect("test input answered");
            assert_eq!(answer.trim(), "ok", "test input {line:?}");
        }
    }

    /// Where the snapshot is written (every second).
    pub fn snapshot_path(&self) -> PathBuf {
        self.home.join("snapshot.png")
    }

    /// A snapshot written after now (RGBA rows), with its size.
    pub fn fresh_snapshot(&self) -> (u32, u32, Vec<u8>) {
        let path = self.snapshot_path();
        let since = std::time::SystemTime::now();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let newer = std::fs::metadata(&path).and_then(|m| m.modified()).is_ok_and(|t| t > since);
            if newer {
                if let Some(image) = read_png(&path) {
                    return image;
                }
            }
            assert!(Instant::now() < deadline, "no new snapshot");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Keep the snapshot as `dest` (for looking at it later).
    pub fn keep_snapshot(&self, dest: &Path) {
        let _ = self.fresh_snapshot();
        let _ = std::fs::copy(self.snapshot_path(), dest);
    }
}

fn read_png(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    let bytes = std::fs::read(path).ok()?;
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    Some((info.width, info.height, buf))
}

fn unique() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

impl Drop for HeadlessSession {
    fn drop(&mut self) {
        // SIGTERM first: the compositor stops the shell and Xwayland.
        // SAFETY: signalling our own child.
        unsafe { libc_kill(self.comp.id() as i32, 15) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.comp.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.comp.kill();
        let _ = self.comp.wait();
        if std::env::var_os("HELIX_TEST_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.home);
        } else {
            eprintln!("kept {}", self.home.display());
        }
    }
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}
