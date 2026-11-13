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

/// The arguments to `bwrap` for running `program` (with `args`) of `m`,
/// whose directory is `dir`.
pub fn bwrap_args(m: &Manifest, dir: &Path, host: &Host, program: &Path, args: &[String]) -> Vec<String> {
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let has = |perm: &str| m.permissions.iter().any(|p| p == perm);
    let mut out: Vec<String> = Vec::new();
    let mut push = |items: &[&str]| out.extend(items.iter().map(|i| i.to_string()));
    push(&["--die-with-parent", "--new-session", "--unshare-all"]);
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
        }
    }

    /// Whether `args` holds `seq` in a row.
    fn has_seq(args: &[String], seq: &[&str]) -> bool {
        args.windows(seq.len()).any(|w| w.iter().zip(seq).all(|(a, b)| a == b))
    }

    #[test]
    fn closed_by_default() {
        let a = bwrap_args(&manifest(&[]), Path::new("/ext/org.x.S"), &host(), Path::new("/ext/org.x.S/bin/s"), &["--x".into()]);
        assert!(has_seq(&a, &["--die-with-parent", "--new-session", "--unshare-all"]));
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
        assert!(has_seq(&home, &["--bind", "/home/u", "/home/u", "--setenv", "HOME", "/home/u"]));
    }

    #[test]
    fn bus_addresses() {
        assert_eq!(bus_path("unix:path=/run/user/1000/bus"), Some("/run/user/1000/bus".into()));
        assert_eq!(bus_path("unix:abstract=/tmp/x,guid=1;unix:path=/tmp/b,guid=2"), Some("/tmp/b".into()));
        assert_eq!(bus_path("tcp:host=x"), None);
    }
}
