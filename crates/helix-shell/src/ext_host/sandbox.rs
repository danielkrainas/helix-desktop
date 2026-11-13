//! Extension sandboxes (helix-desktop-spec `contracts/extensions/README.md`,
//! "Sandboxes", API 1.2): the bubblewrap command line for a sandboxed
//! extension.

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
    /// The session bus socket's path.
    pub session_bus: Option<PathBuf>,
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
        let session_bus = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().and_then(|a| bus_path(&a));
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

/// The socket path of a `unix:path=…` D-Bus address.
pub fn bus_path(address: &str) -> Option<PathBuf> {
    address.split(';').find_map(|a| a.strip_prefix("unix:")?.split(',').find_map(|kv| kv.strip_prefix("path=")).map(PathBuf::from))
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

/// What starts bubblewrap: `helix-extension-sandbox` (installed in
/// `../libexec/helix/` from the shell, or beside it), which AppArmor may
/// allow user namespaces; else `bwrap` itself.
pub fn launcher() -> PathBuf {
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
    exe_dir
        .into_iter()
        .flat_map(|d| [d.join("../libexec/helix").join(LAUNCHER), d.join(LAUNCHER)])
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("bwrap"))
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

/// The arguments to `bwrap` for running `program` (with `args`) of `m`,
/// whose directory is `dir`.
pub fn bwrap_args(m: &Manifest, dir: &Path, host: &Host, program: &Path, args: &[String]) -> Vec<String> {
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
    if has("session-bus") {
        if let Some(bus) = &host.session_bus {
            push(&["--ro-bind", &s(bus), &s(bus)]);
        }
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
            session_bus: Some("/run/user/1000/bus".into()),
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
        let a = bwrap_args(&manifest(&[]), Path::new("/ext/org.x.S"), &host(), Path::new("/ext/org.x.S/bin/s"), &["--x".into()]);
        assert!(has_seq(&a, &["--die-with-parent", "--new-session", "--unshare-all", "--unshare-user", "--disable-userns"]));
        let old = Host { disable_userns: false, ..host() };
        let b = bwrap_args(&manifest(&[]), Path::new("/e"), &old, Path::new("/e/s"), &[]);
        assert!(!b.iter().any(|x| x == "--disable-userns"), "bubblewrap before 0.8");
        assert!(!a.contains(&"--share-net".to_owned()), "no network");
        assert!(has_seq(&a, &["--symlink", "usr/bin", "/bin"]) && has_seq(&a, &["--ro-bind", "/lib64", "/lib64"]));
        assert!(has_seq(&a, &["--ro-bind", "/ext/org.x.S", "/ext/org.x.S"]));
        assert!(has_seq(&a, &["--setenv", "HOME", "/home/u/.local/share/helix/extension-data/org.x.S"]));
        assert!(!a.iter().any(|x| x == "/home/u" || x.contains("wayland") || x.ends_with("/bus") || x.contains("system_bus")), "nothing else of the user's");
        assert!(has_seq(&a, &["--chdir", "/ext/org.x.S", "--", "/ext/org.x.S/bin/s", "--x"]));
        assert_eq!(a.last().map(String::as_str), Some("--x"));
    }

    #[test]
    fn permissions_open_doors() {
        let a = bwrap_args(&manifest(&["network", "files:downloads", "popover", "session-bus", "system-bus"]), Path::new("/e"), &host(), Path::new("/e/s"), &[]);
        assert!(a.contains(&"--share-net".to_owned()));
        assert!(has_seq(&a, &["--bind", "/home/u/Downloads", "/home/u/Downloads"]));
        assert!(has_seq(&a, &["--ro-bind", "/run/user/1000/wayland-0", "/run/user/1000/wayland-0"]));
        assert!(has_seq(&a, &["--ro-bind", "/run/user/1000/bus", "/run/user/1000/bus"]));
        assert!(has_seq(&a, &["--ro-bind", "/run/dbus/system_bus_socket", "/run/dbus/system_bus_socket"]));
        let home = bwrap_args(&manifest(&["files:home"]), Path::new("/e"), &host(), Path::new("/e/s"), &[]);
        assert!(!home.windows(2).any(|w| w[0] == "--ro-bind" && w[1] == "/e/s"), "its own program comes with its directory");
        let outside = bwrap_args(&manifest(&[]), Path::new("/e"), &host(), Path::new("/opt/tool/bin/s"), &[]);
        assert!(has_seq(&outside, &["--ro-bind", "/opt/tool/bin/s", "/opt/tool/bin/s"]));
        assert!(has_seq(&home, &["--bind", "/home/u", "/home/u", "--setenv", "HOME", "/home/u"]));
    }

    #[test]
    fn versions_and_reasons() {
        assert!(version_at_least("bubblewrap 0.9.0\n", (0, 8)));
        assert!(version_at_least("bubblewrap 1.0", (0, 8)));
        assert!(!version_at_least("bubblewrap 0.6.2", (0, 8)));
        assert!(!version_at_least("", (0, 8)));
        let ubuntu = "bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted\n";
        assert!(explain(ubuntu, true).contains("restricts user namespaces (AppArmor)"));
        assert!(!explain(ubuntu, false).contains("AppArmor"));
        assert_eq!(explain("", false), "the sandbox can't start: it failed");
    }

    #[test]
    fn bus_addresses() {
        assert_eq!(bus_path("unix:path=/run/user/1000/bus"), Some("/run/user/1000/bus".into()));
        assert_eq!(bus_path("unix:abstract=/tmp/x,guid=1;unix:path=/tmp/b,guid=2"), Some("/tmp/b".into()));
        assert_eq!(bus_path("tcp:host=x"), None);
    }
}
