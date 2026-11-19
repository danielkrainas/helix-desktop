//! Extension sandboxes (helixdesktop/spec `contracts/extensions/README.md`,
//! "Sandboxes", API 1.2): the bubblewrap command line for a sandboxed
//! extension, and the xdg-dbus-proxy that filters its session bus.

use std::path::{Path, PathBuf};

use super::manifest::Manifest;

/// What the host looks like, for the command line.
#[derive(Debug, Clone, Default)]
pub struct Host {
    pub home: PathBuf,
    pub data_home: PathBuf,
    pub runtime_dir: Option<PathBuf>,
    /// The Wayland socket's path.
    pub wayland: Option<PathBuf>,
    /// The session bus's address (what its proxy connects to).
    pub session_bus: Option<String>,
    pub system_bus: PathBuf,
    /// The user's XDG folders: (name as in `files:<name>`, path).
    pub xdg_dirs: Vec<(String, PathBuf)>,
    /// `/bin`, `/sbin`, `/lib`… as they are: (path, where it links to, or
    /// `None` for a directory). Missing ones are left out.
    pub root_entries: Vec<(PathBuf, Option<PathBuf>)>,
    /// Bubblewrap can turn user namespaces off inside the sandbox.
    pub disable_userns: bool,
}

impl Host {
    /// This machine.
    pub fn current() -> Host {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        let wayland = std::env::var_os("WAYLAND_DISPLAY").map(PathBuf::from).map(|w| if w.is_absolute() { w } else { runtime_dir.clone().unwrap_or_default().join(w) });
        let session_bus = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().filter(|a| !a.is_empty());
        let user_dir = |d: gio::glib::UserDirectory| gio::glib::user_special_dir(d);
        let xdg_dirs = [
            ("documents", gio::glib::UserDirectory::Documents),
            ("downloads", gio::glib::UserDirectory::Downloads),
            ("music", gio::glib::UserDirectory::Music),
            ("pictures", gio::glib::UserDirectory::Pictures),
            ("videos", gio::glib::UserDirectory::Videos),
            ("desktop", gio::glib::UserDirectory::Desktop),
        ]
        .into_iter()
        .filter_map(|(name, d)| user_dir(d).map(|p| (name.to_owned(), p)))
        .collect();
        let root_entries = ["/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32"]
            .iter()
            .filter_map(|p| {
                let path = PathBuf::from(p);
                let meta = std::fs::symlink_metadata(&path).ok()?;
                Some((path.clone(), if meta.file_type().is_symlink() { std::fs::read_link(&path).ok() } else { None }))
            })
            .collect();
        Host {
            home: gio::glib::home_dir(),
            data_home: gio::glib::user_data_dir(),
            runtime_dir,
            wayland,
            session_bus,
            system_bus: PathBuf::from("/run/dbus/system_bus_socket"),
            xdg_dirs,
            root_entries,
            disable_userns: can_disable_userns(),
        }
    }
}

/// The writable home of extension `id`.
pub fn data_dir(host: &Host, id: &str) -> PathBuf {
    host.data_home.join("helix/extension-data").join(id)
}

/// Whether bubblewrap is installed.
pub fn available() -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("bwrap").is_file()))
}

const LAUNCHER: &str = "helix-extension-sandbox";
/// The bubblewrap `helix-extension-sandbox` runs (never one from PATH).
pub const SYSTEM_BWRAP: &str = "/usr/bin/bwrap";

/// What starts bubblewrap: `helix-extension-sandbox` (installed in
/// `../libexec/helix/` from the shell, or beside it), which AppArmor may
/// allow user namespaces; else `bwrap` itself. The helper needs a
/// bubblewrap that can turn user namespaces off (0.8).
pub fn launcher() -> PathBuf {
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
    exe_dir
        .into_iter()
        .flat_map(|d| [d.join("../libexec/helix").join(LAUNCHER), d.join(LAUNCHER)])
        .find(|p| p.is_file() && can_disable_userns())
        .unwrap_or_else(|| PathBuf::from("bwrap"))
}

/// Options `helix-extension-sandbox` refuses: they would give the
/// sandboxed program capabilities or namespaces of its own, or read more
/// options from elsewhere.
const REFUSED: &[&str] = &["--cap-add", "--userns", "--userns2", "--pidns", "--args", "--unshare-user-try", "--share-user"];

/// The bubblewrap command line `helix-extension-sandbox ARGS…` runs:
/// always in a user namespace of its own without further ones
/// (`--unshare-user --disable-userns`), and only with ARGS that end their
/// options with `--` and use none of [`REFUSED`] before it. Anyone can run
/// the helper, and AppArmor lets what it starts use user namespaces.
pub fn launcher_command(args: &[std::ffi::OsString]) -> Result<Vec<std::ffi::OsString>, String> {
    // The options end at the last `--` at the latest: a `--` that is an
    // option's value can't hide what follows it.
    let end = args.iter().rposition(|a| a == "--").ok_or("the command must follow --")?;
    if let Some(bad) = args[..end].iter().find(|a| REFUSED.iter().any(|r| a.as_os_str() == *r)) {
        return Err(format!("{} is not allowed", bad.to_string_lossy()));
    }
    let mut out: Vec<std::ffi::OsString> = ["--unshare-user", "--disable-userns", "--die-with-parent"].iter().map(Into::into).collect();
    out.extend(args.iter().cloned());
    Ok(out)
}

/// Whether this bubblewrap can turn off user namespaces inside the
/// sandbox (`--disable-userns`, bubblewrap 0.8).
fn can_disable_userns() -> bool {
    static CAN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CAN.get_or_init(|| {
        let out = std::process::Command::new("bwrap").arg("--version").output().ok();
        let version = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        version_at_least(&version, (0, 8))
    })
}

/// Whether `bubblewrap X.Y.Z` is at least `min`.
fn version_at_least(text: &str, min: (u32, u32)) -> bool {
    let Some(v) = text.split_whitespace().nth(1) else { return false };
    let mut parts = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0)) >= min
}

/// Why sandboxes can't start here, from bubblewrap's complaint.
pub fn explain(stderr: &str, restricted: bool) -> String {
    let line = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("it failed").trim();
    let denied = line.contains("Operation not permitted") || line.contains("Permission denied");
    if denied && restricted {
        format!("the sandbox can't start: this system restricts user namespaces (AppArmor) and the helix-extension-sandbox profile is not loaded ({line})")
    } else {
        format!("the sandbox can't start: {line}")
    }
}

/// Whether a sandbox starts on this system (tried once): `Err` says why not.
pub fn works() -> Result<(), String> {
    static WORKS: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    WORKS
        .get_or_init(|| {
            let host = Host::current();
            let mut args: Vec<String> = ["--die-with-parent", "--unshare-all", "--ro-bind", "/usr", "/usr"].map(String::from).to_vec();
            for (path, target) in &host.root_entries {
                let (p, t) = (path.to_string_lossy().into_owned(), target.as_ref().map(|t| t.to_string_lossy().into_owned()));
                match t {
                    Some(t) => args.extend(["--symlink".into(), t, p]),
                    None => args.extend(["--ro-bind".into(), p.clone(), p]),
                }
            }
            args.extend(["--proc", "/proc", "--dev", "/dev", "--", "true"].map(String::from));
            let out = std::process::Command::new(launcher()).args(&args).stdin(std::process::Stdio::null()).output().map_err(|e| format!("the sandbox can't start: {e}"))?;
            if out.status.success() {
                return Ok(());
            }
            let restricted = std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns").is_ok_and(|v| v.trim() == "1");
            Err(explain(&String::from_utf8_lossy(&out.stderr), restricted))
        })
        .clone()
}

const PROXY: &str = "xdg-dbus-proxy";
/// The descriptor a sandbox gets its proxy's sync pipe on (3 is the
/// connection to the shell).
pub const PROXY_SYNC_FD: i32 = 4;
/// How long a proxy may take to get ready.
const PROXY_READY: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether xdg-dbus-proxy is installed.
pub fn proxy_available() -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(PROXY).is_file()))
}

/// Where the proxy for run `tag` of extension `id` listens (outside the
/// sandbox): `$XDG_RUNTIME_DIR/helix/extension-bus/<id>-<tag>`.
pub fn proxy_socket(host: &Host, id: &str, tag: &str) -> Option<PathBuf> {
    host.runtime_dir.as_ref().map(|r| r.join("helix/extension-bus").join(format!("{id}-{tag}")))
}

/// xdg-dbus-proxy's options after `ADDRESS PATH` for `m`: filtering, the
/// names under its id, and its `[session-bus]` rules (checked when the
/// manifest was read).
pub fn proxy_rules(m: &Manifest) -> Vec<String> {
    let mut out = vec!["--filter".to_owned()];
    out.extend(super::manifest::own_bus_name(&m.id).map(|n| format!("--own={n}")));
    out.extend(m.permissions.iter().filter_map(|p| super::manifest::bus_rule(p)).map(|(kind, value)| format!("--{kind}={value}")));
    out
}

/// A sandbox's session bus proxy to start: where it listens, the bus,
/// and its options.
#[derive(Debug, Clone, PartialEq)]
pub struct ProxyPlan {
    pub socket: PathBuf,
    pub address: String,
    pub rules: Vec<String>,
}

/// Start xdg-dbus-proxy for `plan` and wait until it is ready (it says so
/// on its `--fd`). It stops when the read end of that pipe, returned for
/// the sandbox to hold (`--sync-fd`), is closed everywhere, and when the
/// child is dropped.
pub async fn start_proxy(plan: &ProxyPlan) -> std::io::Result<(tokio::process::Child, std::os::fd::OwnedFd)> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = plan.socket.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let _ = std::fs::remove_file(&plan.socket);
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for both ends.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both were just made, and are ours alone.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let mut cmd = tokio::process::Command::new(PROXY);
    cmd.arg("--fd=3").arg(&plan.address).arg(&plan.socket).args(&plan.rules);
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    let fd = write.as_raw_fd();
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if fd != 3 && libc::dup2(fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = libc::fcntl(3, libc::F_GETFD);
            if flags < 0 || libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    drop(write);
    let ready = tokio::task::spawn_blocking(move || wait_ready(read, PROXY_READY)).await.map_err(std::io::Error::other)?;
    match ready {
        Ok(read) => Ok((child, read)),
        Err(err) => {
            let _ = child.start_kill();
            let mut complaint = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                use tokio::io::AsyncReadExt;
                let _ = tokio::time::timeout(std::time::Duration::from_secs(1), stderr.read_to_string(&mut complaint)).await;
            }
            let _ = std::fs::remove_file(&plan.socket);
            let line = complaint.lines().find(|l| !l.trim().is_empty()).map_or_else(|| err.to_string(), |l| l.trim().to_owned());
            Err(std::io::Error::other(format!("the session bus proxy did not start: {line}")))
        }
    }
}

/// Wait up to `timeout` for a byte on `read` (a proxy's `--fd`).
fn wait_ready(read: std::os::fd::OwnedFd, timeout: std::time::Duration) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::AsRawFd;
    let mut poll = libc::pollfd { fd: read.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut poll, 1, left.as_millis().min(i32::MAX as u128) as i32) };
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "not ready in time"));
        }
        let mut byte = 0u8;
        // SAFETY: reading one byte into `byte`.
        let got = unsafe { libc::read(read.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
        return match got {
            1 => Ok(read),
            0 => Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "it exited")),
            _ => Err(std::io::Error::last_os_error()),
        };
    }
}

/// The arguments to `bwrap` for running `program` (with `args`) of `m`,
/// whose directory is `dir`; `bus` is the socket of its session bus
/// proxy, whose sync pipe it gets as descriptor [`PROXY_SYNC_FD`].
pub fn bwrap_args(m: &Manifest, dir: &Path, host: &Host, program: &Path, args: &[String], bus: Option<&Path>) -> Vec<String> {
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let has = |perm: &str| m.permissions.iter().any(|p| p == perm);
    let mut out: Vec<String> = Vec::new();
    let mut push = |items: &[&str]| out.extend(items.iter().map(|i| i.to_string()));
    push(&["--die-with-parent", "--new-session", "--unshare-all"]);
    // No user namespaces of its own inside.
    if host.disable_userns {
        push(&["--unshare-user", "--disable-userns"]);
    }
    if has("network") {
        push(&["--share-net"]);
    }
    push(&["--ro-bind", "/usr", "/usr", "--ro-bind", "/etc", "/etc"]);
    for (path, target) in &host.root_entries {
        match target {
            Some(t) => push(&["--symlink", &s(t), &s(path)]),
            None => push(&["--ro-bind", &s(path), &s(path)]),
        }
    }
    push(&["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
    // Its own directory, read-only, and its own home.
    push(&["--ro-bind", &s(dir), &s(dir)]);
    // A program outside it (an absolute `exec`), read-only too.
    if !program.starts_with(dir) {
        push(&["--ro-bind", &s(program), &s(program)]);
    }
    let data = data_dir(host, &m.id);
    let full_home = has("files:home");
    if full_home {
        push(&["--bind", &s(&host.home), &s(&host.home), "--setenv", "HOME", &s(&host.home)]);
    } else {
        push(&["--bind", &s(&data), &s(&data), "--setenv", "HOME", &s(&data)]);
    }
    if !full_home {
        for (name, path) in &host.xdg_dirs {
            if has(&format!("files:{name}")) {
                push(&["--bind", &s(path), &s(path)]);
            }
        }
    }
    // An empty runtime directory; the sockets it may use.
    if let Some(runtime) = &host.runtime_dir {
        push(&["--dir", &s(runtime), "--setenv", "XDG_RUNTIME_DIR", &s(runtime)]);
    }
    if has("popover") {
        if let Some(w) = &host.wayland {
            push(&["--ro-bind", &s(w), &s(w)]);
        }
    }
    // The session bus only through its proxy (never the bus itself), at
    // the usual place; it ends with the sandbox.
    match (bus, &host.runtime_dir) {
        (Some(bus), Some(runtime)) if has("session-bus") => {
            let inside = runtime.join("bus");
            push(&["--ro-bind", &s(bus), &s(&inside), "--setenv", "DBUS_SESSION_BUS_ADDRESS", &format!("unix:path={}", s(&inside))]);
            push(&["--sync-fd", &PROXY_SYNC_FD.to_string()]);
        }
        _ => push(&["--unsetenv", "DBUS_SESSION_BUS_ADDRESS"]),
    }
    if has("system-bus") {
        push(&["--ro-bind", &s(&host.system_bus), &s(&host.system_bus)]);
    }
    push(&["--chdir", &s(dir), "--"]);
    out.push(s(program));
    out.extend(args.iter().cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(permissions: &[&str]) -> Manifest {
        let source = format!(
            "id = \"org.x.S\"\nname = \"S\"\nversion = \"1\"\napi = \"1.2\"\nexec = [\"bin/s\"]\nsandbox = true\npermissions = [{}]\n",
            permissions.iter().map(|p| format!("\"{p}\"")).collect::<Vec<_>>().join(", ")
        );
        super::super::manifest::parse(&source, None).unwrap()
    }

    fn host() -> Host {
        Host {
            home: "/home/u".into(),
            data_home: "/home/u/.local/share".into(),
            runtime_dir: Some("/run/user/1000".into()),
            wayland: Some("/run/user/1000/wayland-0".into()),
            session_bus: Some("unix:path=/run/user/1000/bus".into()),
            system_bus: "/run/dbus/system_bus_socket".into(),
            xdg_dirs: vec![("downloads".into(), "/home/u/Downloads".into())],
            root_entries: vec![("/bin".into(), Some("usr/bin".into())), ("/lib64".into(), None)],
            disable_userns: true,
        }
    }

    /// Whether `args` holds `seq` in a row.
    fn has_seq(args: &[String], seq: &[&str]) -> bool {
        args.windows(seq.len()).any(|w| w.iter().zip(seq).all(|(a, b)| a == b))
    }

    #[test]
    fn closed_by_default() {
        let a = bwrap_args(&manifest(&[]), Path::new("/ext/org.x.S"), &host(), Path::new("/ext/org.x.S/bin/s"), &["--x".into()], None);
        assert!(has_seq(&a, &["--die-with-parent", "--new-session", "--unshare-all", "--unshare-user", "--disable-userns"]));
        let old = Host { disable_userns: false, ..host() };
        let b = bwrap_args(&manifest(&[]), Path::new("/e"), &old, Path::new("/e/s"), &[], None);
        assert!(!b.iter().any(|x| x == "--disable-userns"), "bubblewrap before 0.8");
        assert!(!a.contains(&"--share-net".to_owned()), "no network");
        assert!(has_seq(&a, &["--symlink", "usr/bin", "/bin"]) && has_seq(&a, &["--ro-bind", "/lib64", "/lib64"]));
        assert!(has_seq(&a, &["--ro-bind", "/ext/org.x.S", "/ext/org.x.S"]));
        assert!(has_seq(&a, &["--setenv", "HOME", "/home/u/.local/share/helix/extension-data/org.x.S"]));
        assert!(!a.iter().any(|x| x == "/home/u" || x.contains("wayland") || x.ends_with("/bus") || x.contains("system_bus")), "nothing else of the user's");
        assert!(has_seq(&a, &["--unsetenv", "DBUS_SESSION_BUS_ADDRESS"]) && !a.iter().any(|x| x == "--sync-fd"));
        // No bus without the permission, even with a proxy.
        let c = bwrap_args(&manifest(&[]), Path::new("/e"), &host(), Path::new("/e/s"), &[], Some(Path::new("/p")));
        assert!(!c.iter().any(|x| x == "/p") && has_seq(&c, &["--unsetenv", "DBUS_SESSION_BUS_ADDRESS"]));
        assert!(has_seq(&a, &["--chdir", "/ext/org.x.S", "--", "/ext/org.x.S/bin/s", "--x"]));
        assert_eq!(a.last().map(String::as_str), Some("--x"));
    }

    #[test]
    fn permissions_open_doors() {
        let proxy = Path::new("/run/user/1000/helix/extension-bus/org.x.S-1");
        let a = bwrap_args(&manifest(&["network", "files:downloads", "popover", "session-bus", "system-bus"]), Path::new("/e"), &host(), Path::new("/e/s"), &[], Some(proxy));
        assert!(a.contains(&"--share-net".to_owned()));
        assert!(has_seq(&a, &["--bind", "/home/u/Downloads", "/home/u/Downloads"]));
        assert!(has_seq(&a, &["--ro-bind", "/run/user/1000/wayland-0", "/run/user/1000/wayland-0"]));
        // The proxy's socket, never the bus's.
        assert!(has_seq(&a, &["--ro-bind", "/run/user/1000/helix/extension-bus/org.x.S-1", "/run/user/1000/bus", "--setenv", "DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus", "--sync-fd", "4"]));
        assert!(!has_seq(&a, &["--ro-bind", "/run/user/1000/bus"]));
        assert!(has_seq(&a, &["--ro-bind", "/run/dbus/system_bus_socket", "/run/dbus/system_bus_socket"]));
        let home = bwrap_args(&manifest(&["files:home"]), Path::new("/e"), &host(), Path::new("/e/s"), &[], None);
        assert!(!home.windows(2).any(|w| w[0] == "--ro-bind" && w[1] == "/e/s"), "its own program comes with its directory");
        let outside = bwrap_args(&manifest(&[]), Path::new("/e"), &host(), Path::new("/opt/tool/bin/s"), &[], None);
        assert!(has_seq(&outside, &["--ro-bind", "/opt/tool/bin/s", "/opt/tool/bin/s"]));
        assert!(has_seq(&home, &["--bind", "/home/u", "/home/u", "--setenv", "HOME", "/home/u"]));
    }

    #[test]
    fn proxy_options() {
        let source = "id = \"org.x.S\"\nname = \"S\"\nversion = \"1\"\napi = \"1.2\"\nexec = [\"bin/s\"]\nsandbox = true\npermissions = [\"session-bus\"]\n[session-bus]\ntalk = [\"org.example.A\"]\nsee = [\"org.example.B.*\"]\nown = [\"org.example.C\"]\ncall = [\"org.example.D=org.example.D.Get@/d\"]\nbroadcast = [\"org.example.D=org.example.D.*@/d/*\"]\n";
        let m = super::super::manifest::parse(source, None).unwrap();
        assert_eq!(
            proxy_rules(&m),
            ["--filter", "--own=org.x.S.*", "--see=org.example.B.*", "--talk=org.example.A", "--own=org.example.C", "--call=org.example.D=org.example.D.Get@/d", "--broadcast=org.example.D=org.example.D.*@/d/*"]
        );
        assert_eq!(proxy_rules(&manifest(&["session-bus"])), ["--filter", "--own=org.x.S.*"], "its own names only");
        assert_eq!(proxy_socket(&host(), "org.x.S", "7-2"), Some("/run/user/1000/helix/extension-bus/org.x.S-7-2".into()));
        assert_eq!(proxy_socket(&Host::default(), "org.x.S", "1"), None);
    }

    #[test]
    fn versions_and_reasons() {
        assert!(version_at_least("bubblewrap 0.9.0\n", (0, 8)));
        assert!(version_at_least("bubblewrap 1.0", (0, 8)));
        assert!(!version_at_least("bubblewrap 0.6.2", (0, 8)));
        assert!(!version_at_least("", (0, 8)));
    }

    #[test]
    fn launcher_commands() {
        let os = |a: &[&str]| a.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        let ok = launcher_command(&os(&["--ro-bind", "/usr", "/usr", "--", "/bin/true", "--cap-add"])).unwrap();
        assert_eq!(ok[..3], os(&["--unshare-user", "--disable-userns", "--die-with-parent"])[..]);
        assert_eq!(ok[3..], os(&["--ro-bind", "/usr", "/usr", "--", "/bin/true", "--cap-add"])[..], "the program's own arguments are its business");
        assert!(launcher_command(&os(&["--ro-bind", "/", "/", "sh"])).is_err(), "no --");
        assert!(launcher_command(&os(&["--cap-add", "ALL", "--", "sh"])).is_err());
        assert!(launcher_command(&os(&["--args", "3", "--", "sh"])).is_err());
        assert!(launcher_command(&os(&["--userns", "3", "--", "sh"])).is_err());
        // A `--` given as a value does not end the options.
        assert!(launcher_command(&os(&["--chdir", "--", "--cap-add", "ALL", "--", "sh"])).is_err());
        assert!(launcher_command(&os(&["--chdir", "--", "--cap-add", "ALL", "--", "sh", "--"])).is_err());
        let ubuntu = "bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted\n";
        assert!(explain(ubuntu, true).contains("restricts user namespaces (AppArmor)"));
        assert!(!explain(ubuntu, false).contains("AppArmor"));
        assert_eq!(explain("", false), "the sandbox can't start: it failed");
    }
}
