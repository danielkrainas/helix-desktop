//! A sandboxed extension's session bus (helixdesktop/spec
//! `contracts/extensions/README.md`, "Sandboxes"): it goes through
//! xdg-dbus-proxy, which lets the extension reach the names its
//! `[session-bus]` rules allow and own the names under its id, and
//! nothing else: not other services, not the shell's
//! `org.helixdesktop.Extensions1`, not the bus's own
//! `UpdateActivationEnvironment`.
//!
//! Needs bubblewrap that can start a sandbox here, xdg-dbus-proxy and
//! dbus-send; skipped without them.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use helix_shell::ext_host::manager::{Manager, OBJECT_PATH};
use helix_shell::ext_host::ExtHost;
use helix_test_support::TestBus;

struct Pinged(Arc<AtomicU32>);

#[zbus::interface(name = "org.example.Test")]
impl Pinged {
    fn ping(&self) -> String {
        self.0.fetch_add(1, Ordering::SeqCst);
        "pong".into()
    }
}

const MANIFEST: &str = r#"
id = "org.example.BusExt"
name = "Bus"
version = "1"
api = "1.2"
sandbox = true
exec = ["/usr/bin/sh", "run.sh"]
permissions = ["session-bus"]

[session-bus]
talk = ["org.example.Allowed"]
"#;

/// The extension: what it can reach, one line each, then "done".
const SCRIPT: &str = r#"
try() { label=$1; shift; if dbus-send --session --print-reply --reply-timeout=3000 "$@" >/dev/null 2>&1; then echo "$label yes"; else echo "$label no"; fi; }
owns() { dbus-send --session --print-reply --dest=org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus.RequestName string:$1 uint32:4 2>/dev/null | grep -q 'uint32 1' && echo "$2 yes" || echo "$2 no"; }
echo "address $DBUS_SESSION_BUS_ADDRESS"
try allowed --dest=org.example.Allowed /org/example org.example.Test.Ping
try denied --dest=org.example.Denied /org/example org.example.Test.Ping
try extensions --dest=org.gnome.Shell /org/helixdesktop/Extensions1 org.helixdesktop.Extensions1.ListExtensions
try environment --dest=org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus.UpdateActivationEnvironment dict:string:string:LD_PRELOAD,/x.so
owns org.example.BusExt.Helper own-name
owns org.example.Other other-name
if [ -e "$REAL_BUS" ]; then echo "real-bus yes"; else echo "real-bus no"; fi
echo done
sleep 60
"#;

#[tokio::test(flavor = "multi_thread")]
async fn filtered_session_bus() {
    for tool in ["/usr/bin/bwrap", "/usr/bin/xdg-dbus-proxy", "/usr/bin/dbus-send"] {
        if !Path::new(tool).exists() {
            eprintln!("{tool} is not installed; skipping");
            return;
        }
    }
    let Some(bus) = TestBus::start() else { return };
    let tmp = std::env::temp_dir().join(format!("helix-ext-bus-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let runtime = tmp.join("run");
    std::fs::create_dir_all(&runtime).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let ext_dir = tmp.join("ext");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(ext_dir.join("extension.toml"), MANIFEST).unwrap();
    let real_bus = bus_path(bus.address());
    std::fs::write(ext_dir.join("run.sh"), format!("REAL_BUS={real_bus}\n{SCRIPT}")).unwrap();
    // SAFETY: this test binary runs this one test, before any GLib use.
    unsafe {
        for (key, dir) in [("HOME", "home"), ("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache"), ("XDG_DATA_DIRS", "none")] {
            std::env::set_var(key, tmp.join(dir));
        }
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", bus.address());
        std::env::set_var("GSETTINGS_BACKEND", "memory");
        std::env::set_var("PATH", "/usr/bin:/bin");
        std::env::remove_var("WAYLAND_DISPLAY");
    }
    if let Err(reason) = helix_shell::ext_host::sandbox::works() {
        eprintln!("{reason}; skipping");
        return;
    }

    // The services: one it may talk to, one it may not, and the shell's.
    let allowed = Arc::new(AtomicU32::new(0));
    let denied = Arc::new(AtomicU32::new(0));
    let _a = bus.builder().serve_at("/org/example", Pinged(allowed.clone())).unwrap().name("org.example.Allowed").unwrap().build().await.unwrap();
    let _b = bus.builder().serve_at("/org/example", Pinged(denied.clone())).unwrap().name("org.example.Denied").unwrap().build().await.unwrap();
    let (ui, _rx) = tokio::sync::mpsc::unbounded_channel();
    let host = ExtHost::new(ui);
    let _shell = bus.builder().serve_at(OBJECT_PATH, Manager { host: host.clone() }).unwrap().name("org.gnome.Shell").unwrap().build().await.unwrap();

    let id = host.run_development(&ext_dir, String::new()).unwrap();
    assert_eq!(id, "org.example.BusExt");
    let log = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let lines = host.log_lines(&id).unwrap_or_default();
            if lines.iter().any(|l| l == "done") || lines.iter().any(|l| l.starts_with("cannot start")) {
                return lines;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the extension reports in time");
    let said = |label: &str| log.iter().find_map(|l| l.strip_prefix(&format!("{label} "))).unwrap_or_else(|| panic!("no {label} in {log:#?}")).to_owned();
    assert_eq!(said("address"), format!("unix:path={}", runtime.join("bus").display()), "the proxy, where a bus usually is");
    assert_eq!(said("allowed"), "yes", "{log:#?}");
    assert_eq!(said("denied"), "no");
    assert_eq!(said("extensions"), "no", "the shell's controls are out of reach");
    assert_eq!(said("environment"), "no", "the bus's activation environment too");
    assert_eq!(said("own-name"), "yes", "names under its id are its own");
    assert_eq!(said("other-name"), "no");
    assert_eq!(said("real-bus"), "no", "the bus's own socket is not in the sandbox");
    assert_eq!(allowed.load(Ordering::SeqCst), 1);
    assert_eq!(denied.load(Ordering::SeqCst), 0, "nothing reached the other service");

    // Stopped, its proxy goes too.
    let proxies = runtime.join("helix/extension-bus");
    assert_eq!(std::fs::read_dir(&proxies).unwrap().count(), 1, "one proxy socket while it runs");
    host.stop_development(&id).unwrap();
    let gone = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let sockets = std::fs::read_dir(&proxies).map(|d| d.count()).unwrap_or(0);
            if sockets == 0 && proxy_pids(&runtime).is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(gone.is_ok(), "the proxy ended with the extension: {:?}", proxy_pids(&runtime));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The socket path of the test bus's `unix:path=` address.
fn bus_path(address: &str) -> String {
    address.strip_prefix("unix:path=").map(|p| p.split(',').next().unwrap_or(p).to_owned()).expect("a unix:path= address")
}

/// xdg-dbus-proxy processes whose socket is under `runtime`.
fn proxy_pids(runtime: &Path) -> Vec<u32> {
    let want = runtime.to_string_lossy().into_owned();
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let cmdline = String::from_utf8_lossy(&cmdline);
            cmdline.starts_with("xdg-dbus-proxy") && cmdline.contains(&want)
        })
        .collect()
}
