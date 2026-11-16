//! The login screen's monitor layout: `helix-login-monitors`, run as root
//! through `pkexec` (polkit action `org.helix.login-screen-monitors`),
//! takes one remembered monitor configuration on standard input, checks
//! it, and puts it in the system-wide file the greeter's compositor reads
//! ([`SYSTEM_FILE`]), keeping that file's configurations for other sets of
//! monitors.
//!
//! Spec: helix-desktop-spec `components/greeter.md`, "The monitor", and
//! `contracts/dbus/org.gnome.Mutter.DisplayConfig.md`, "Remembered
//! configurations".
//!
//! The input is in the remembered configurations' own format: exactly one
//! `[[configuration]]`, whose `monitors` names the set of monitors
//! connected now (as [`Layout::key`] writes it) and whose logical monitors
//! show only those. Nothing else is accepted: unknown keys, more or fewer
//! configurations, or a layout the compositor would refuse
//! ([`Layout::check`]). The file is written anew from what was parsed,
//! never copied, so it holds monitor configurations only.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use serde::Deserialize;

use crate::display_config::{
    Layout, LogicalConfig, Mode, Monitor, MonitorChoice, MonitorsConfig, StoreFile, StoredConfig, ALL_TRANSFORMS,
};

/// Where the greeter's compositor (and every session without a
/// configuration of its own for the monitors) finds it.
pub const SYSTEM_FILE: &str = "/etc/xdg/helix/monitors.toml";
/// The most input read.
pub const MAX_INPUT: usize = 64 * 1024;
const MAX_MONITORS: usize = 16;
const MAX_FIELD: usize = 128;
/// Positions and mode sizes, in pixels.
const MAX_COORDINATE: i32 = 1 << 18;
const MAX_SIZE: i32 = 1 << 15;
const MAX_REFRESH_MHZ: i32 = 1_000_000;

/// Why nothing was written.
#[derive(Debug, PartialEq)]
pub enum Error {
    /// The input is not one acceptable configuration (exit status 1).
    Invalid(String),
    /// The system file could not be read or written (exit status 2).
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Invalid(e) => write!(f, "invalid configuration: {e}"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

// The input, strictly: unknown keys are refused.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    configuration: Vec<InputConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputConfig {
    monitors: String,
    logical: Vec<InputLogical>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputLogical {
    x: i32,
    y: i32,
    scale: f64,
    transform: u32,
    primary: bool,
    monitors: Vec<InputChoice>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputChoice {
    connector: String,
    width: i32,
    height: i32,
    refresh_mhz: i32,
    #[serde(default)]
    color_mode: u32,
    #[serde(default)]
    underscan: bool,
}

fn invalid(e: impl Into<String>) -> Error {
    Error::Invalid(e.into())
}

/// One field of a monitor in the set: printable, short, and free of the
/// separators.
fn field(value: &str, what: &str) -> Result<(), Error> {
    if value.len() > MAX_FIELD || value.chars().any(|c| c.is_control() || c == '|' || c == ';') {
        return Err(invalid(format!("bad monitor {what}")));
    }
    Ok(())
}

fn connector_ok(c: &str) -> bool {
    !c.is_empty() && c.len() <= 64 && c.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Check `input`; returns the set of monitors' key and the configuration.
pub fn parse(input: &[u8]) -> Result<(String, MonitorsConfig), Error> {
    if input.len() > MAX_INPUT {
        return Err(invalid("too long"));
    }
    let text = std::str::from_utf8(input).map_err(|_| invalid("not UTF-8"))?;
    let input: Input = toml::from_str(text).map_err(|e| invalid(e.message().to_owned()))?;
    let [entry] = <[InputConfig; 1]>::try_from(input.configuration).map_err(|v| invalid(format!("{} configurations, not one", v.len())))?;

    // The set of monitors, as the compositor names it.
    let mut monitors = Vec::new();
    for spec in entry.monitors.split(';') {
        let parts: Vec<&str> = spec.split('|').collect();
        let [connector, vendor, product, serial] = parts[..] else {
            return Err(invalid("bad set of monitors"));
        };
        if !connector_ok(connector) {
            return Err(invalid("bad connector name"));
        }
        field(vendor, "vendor")?;
        field(product, "product")?;
        field(serial, "serial")?;
        if monitors.iter().any(|m: &Monitor| m.connector == connector) {
            return Err(invalid(format!("{connector} is listed twice")));
        }
        monitors.push(Monitor {
            connector: connector.into(),
            vendor: vendor.into(),
            product: product.into(),
            serial: serial.into(),
            display_name: String::new(),
            builtin: false,
            modes: Vec::new(),
            x: 0,
            y: 0,
            scale: 1.0,
            transform: 0,
            primary: false,
            transforms: ALL_TRANSFORMS.to_vec(),
            hdr: true,
            color_mode: 0,
            underscan: Some(false),
        });
    }
    if monitors.len() > MAX_MONITORS {
        return Err(invalid("too many monitors"));
    }

    let mut config = MonitorsConfig::default();
    for l in entry.logical {
        if l.x.unsigned_abs() > MAX_COORDINATE as u32 || l.y.unsigned_abs() > MAX_COORDINATE as u32 {
            return Err(invalid("position out of range"));
        }
        if !l.scale.is_finite() {
            return Err(invalid("bad scale"));
        }
        let mut choices = Vec::new();
        for c in l.monitors {
            if !(1..=MAX_SIZE).contains(&c.width) || !(1..=MAX_SIZE).contains(&c.height) || !(1..=MAX_REFRESH_MHZ).contains(&c.refresh_mhz) {
                return Err(invalid("mode out of range"));
            }
            if c.color_mode > 1 {
                return Err(invalid("bad color mode"));
            }
            // The modes the monitors have are not known here: each has the
            // one chosen for it.
            let monitor = monitors
                .iter_mut()
                .find(|m| m.connector == c.connector)
                .ok_or_else(|| invalid(format!("{} is not in the set of monitors", c.connector)))?;
            monitor.modes.push(Mode { width: c.width, height: c.height, refresh_mhz: c.refresh_mhz, preferred: false, current: false });
            choices.push(MonitorChoice {
                connector: c.connector,
                width: c.width,
                height: c.height,
                refresh_mhz: c.refresh_mhz,
                color_mode: c.color_mode,
                underscan: c.underscan,
            });
        }
        config.logical.push(LogicalConfig { x: l.x, y: l.y, scale: l.scale, transform: l.transform, primary: l.primary, monitors: choices });
    }
    if config.logical.len() > MAX_MONITORS {
        return Err(invalid("too many logical monitors"));
    }
    let layout = Layout::new(monitors);
    if layout.key() != entry.monitors {
        return Err(invalid("the set of monitors is not in the compositor's order"));
    }
    layout.check(&config).map_err(invalid)?;
    Ok((entry.monitors, config))
}

/// Check `input` and put it in `target` (normally [`SYSTEM_FILE`]),
/// replacing the configuration for the same monitors and keeping the
/// others. `target` is replaced atomically, mode 0644 (owned by root when
/// run as root).
pub fn install(input: &[u8], target: &Path) -> Result<(), Error> {
    let (key, config) = parse(input)?;
    let mut file = match std::fs::read_to_string(target) {
        Ok(text) => toml::from_str::<StoreFile>(&text)
            .map_err(|e| Error::Io(format!("{} cannot be read ({}); it was left as it is", target.display(), e.message())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => StoreFile::default(),
        Err(e) => return Err(Error::Io(format!("cannot read {}: {e}", target.display()))),
    };
    file.configuration.retain(|c| c.monitors != key);
    file.configuration.push(StoredConfig { monitors: key, logical: config.logical });
    let text = toml::to_string(&file).map_err(|e| Error::Io(e.to_string()))?;
    let text = format!(
        "# Monitor layouts for the login screen and for users without their own,\n# one per set of monitors (written by helix-login-monitors).\n\n{text}"
    );
    write_atomically(target, text.as_bytes()).map_err(|e| Error::Io(format!("cannot write {}: {e}", target.display())))
}

/// Write `contents` to a new file next to `target`, then rename it over
/// `target`.
fn write_atomically(target: &Path, contents: &[u8]) -> std::io::Result<()> {
    let dir = target.parent().ok_or_else(|| std::io::Error::other("no directory"))?;
    std::fs::DirBuilder::new().recursive(true).mode(0o755).create(dir)?;
    let name = target.file_name().ok_or_else(|| std::io::Error::other("no file name"))?;
    let tmp = dir.join(format!(".{}.{}", name.to_string_lossy(), std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o644).open(&tmp)?;
        // Whatever the umask.
        file.set_permissions(std::fs::Permissions::from_mode(0o644))?;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            std::os::unix::fs::fchown(&file, Some(0), Some(0))?;
        }
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&tmp, target)?;
        std::fs::File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_config::Store;

    const SET: &str = "DP-3|DEL|U2720Q|ABC123;DVI-D-1|SAM|S24|0";

    /// DP-3 primary on the left at 2x, DVI-D-1 on its right.
    fn input(set: &str, extra: &str) -> String {
        format!(
            r#"[[configuration]]
monitors = "{set}"
{extra}
[[configuration.logical]]
x = 0
y = 0
scale = 2.0
transform = 0
primary = true

[[configuration.logical.monitors]]
connector = "DP-3"
width = 3840
height = 2160
refresh_mhz = 60000

[[configuration.logical]]
x = 1920
y = 0
scale = 1.0
transform = 0
primary = false

[[configuration.logical.monitors]]
connector = "DVI-D-1"
width = 1920
height = 1080
refresh_mhz = 59940
color_mode = 0
underscan = false
"#
        )
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("helix-login-monitors-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn accepts_a_configuration() {
        let (key, config) = parse(input(SET, "").as_bytes()).unwrap();
        assert_eq!(key, SET);
        assert_eq!(config.logical.len(), 2);
        assert!(config.logical[0].primary);
        assert_eq!(config.logical[0].monitors[0].connector, "DP-3");
    }

    #[test]
    fn rejects_invalid_input() {
        let bad = |text: String, why: &str| {
            assert!(matches!(parse(text.as_bytes()), Err(Error::Invalid(_))), "{why}: {text}");
        };
        bad(String::new(), "empty");
        bad("not toml [".into(), "not TOML");
        bad(input(SET, "comment = \"x\""), "unknown key");
        bad(input(SET, "").replace("refresh_mhz = 60000", "refresh_mhz = 60000\nshell = \"/bin/sh\""), "unknown key in a monitor");
        bad(format!("{}\n{}", input(SET, ""), input(SET, "")), "two configurations");
        bad("[[configuration]]\n".into(), "incomplete");
        bad(input("DVI-D-1|SAM|S24|0;DP-3|DEL|U2720Q|ABC123", ""), "set not in order");
        bad(input("DP-3|DEL|U2720Q|ABC123", ""), "a monitor outside the set");
        bad(input("DP-3|DEL|U2720Q;DVI-D-1|SAM|S24|0", ""), "three fields");
        bad(input("DP-3|DEL|U27\u{7}|ABC;DVI-D-1|SAM|S24|0", ""), "control character");
        bad(input("DP 3|DEL|U2720Q|ABC123;DVI-D-1|SAM|S24|0", "").replace("\"DP-3\"", "\"DP 3\""), "bad connector");
        bad(input(SET, "").replace("primary = false", "primary = true"), "two primaries");
        bad(input(SET, "").replace("x = 1920", "x = 1000"), "overlapping");
        bad(input(SET, "").replace("x = 1920", "x = 2000"), "not touching");
        bad(input(SET, "").replace("scale = 2.0", "scale = 1.7"), "unsupported scale");
        bad(input(SET, "").replace("scale = 2.0", "scale = nan"), "NaN scale");
        bad(input(SET, "").replace("transform = 0\nprimary = true", "transform = 9\nprimary = true"), "bad transform");
        bad(input(SET, "").replace("color_mode = 0", "color_mode = 7"), "bad color mode");
        bad(input(SET, "").replace("width = 3840", "width = 0"), "empty mode");
        bad(input(SET, "").replace("x = 1920", "x = 2147483000"), "huge position");
        bad(input(SET, "").replace("connector = \"DVI-D-1\"", "connector = \"DP-3\""), "a monitor twice");
        bad(format!("{}{}", input(SET, ""), " ".repeat(MAX_INPUT)), "too long");
        assert!(matches!(parse(&[0xff, 0xfe]), Err(Error::Invalid(_))), "not UTF-8");
    }

    #[test]
    fn writes_atomically_with_mode_644() {
        let dir = scratch("write");
        let target = dir.join("etc/xdg/helix/monitors.toml");
        install(input(SET, "").as_bytes(), &target).unwrap();
        let meta = std::fs::metadata(&target).unwrap();
        assert_eq!(meta.permissions().mode() & 0o7777, 0o644);
        assert_eq!(std::fs::metadata(target.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o755);
        let names: Vec<_> = std::fs::read_dir(target.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["monitors.toml"], "no temporary file left");
        // The compositor reads it back.
        let (key, config) = parse(input(SET, "").as_bytes()).unwrap();
        assert_eq!(Store::at(target.clone()).load(&key), Some(config));

        // Replacing it is a rename: a reader holding the old file still
        // sees the old contents.
        let old = std::fs::File::open(&target).unwrap();
        let old_inode = std::os::unix::fs::MetadataExt::ino(&old.metadata().unwrap());
        install(input(SET, "").replace("scale = 2.0", "scale = 1.0").replace("x = 1920", "x = 3840").as_bytes(), &target).unwrap();
        let new_inode = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&target).unwrap());
        assert_ne!(old_inode, new_inode, "replaced, not rewritten in place");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copies_only_the_current_set_and_keeps_others() {
        let dir = scratch("merge");
        let target = dir.join("monitors.toml");
        let laptop = Store::at(target.clone());
        let docked = crate::display_config::MonitorsConfig {
            logical: vec![LogicalConfig {
                x: 0,
                y: 0,
                scale: 1.0,
                transform: 0,
                primary: true,
                monitors: vec![MonitorChoice { connector: "eDP-1".into(), width: 1920, height: 1080, refresh_mhz: 60_000, color_mode: 0, underscan: false }],
            }],
        };
        laptop.save("eDP-1|BOE|Panel|0", &docked).unwrap();
        // An older layout for the same set goes.
        install(input(SET, "").replace("scale = 2.0", "scale = 1.0").replace("x = 1920", "x = 3840").as_bytes(), &target).unwrap();
        install(input(SET, "").as_bytes(), &target).unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        assert_eq!(text.matches("[[configuration]]").count(), 2, "{text}");
        assert_eq!(laptop.load("eDP-1|BOE|Panel|0"), Some(docked), "other sets kept");
        assert_eq!(laptop.load(SET), Some(parse(input(SET, "").as_bytes()).unwrap().1));

        // Invalid input leaves the file alone.
        let before = std::fs::read(&target).unwrap();
        assert!(matches!(install(b"[[configuration]]\nmonitors = 1\n", &target), Err(Error::Invalid(_))));
        assert_eq!(std::fs::read(&target).unwrap(), before);
        // So does an existing file that can't be read.
        std::fs::write(&target, "garbage [").unwrap();
        assert!(matches!(install(input(SET, "").as_bytes(), &target), Err(Error::Io(_))));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "garbage [");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
