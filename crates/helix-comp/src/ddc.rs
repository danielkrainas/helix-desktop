//! External monitors' brightness over DDC/CI (helix-desktop-spec
//! `org.gnome.Mutter.DisplayConfig.md`, "Backlight"): the MCCS brightness
//! control (VCP code 0x10), read and written over the I²C bus the kernel
//! exposes for each DRM connector.
//!
//! Message framing follows the DDC/CI standard: the host writes to I²C
//! address 0x37 a source byte (0x51), a length byte (0x80 | n), n data
//! bytes and a checksum (XOR of the destination address 0x6E and every
//! byte sent); replies carry 0x6E, a length, data and a checksum computed
//! from the virtual host address 0x50.
//!
//! Not yet tried with real monitors: everything here is checked against a
//! simulated monitor in the unit tests.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// The lowest brightness offered for an external monitor (its backlight
/// is never fully off at 0).
pub const MIN: i32 = 0;

/// The MCCS brightness control.
pub const BRIGHTNESS: u8 = 0x10;
/// The monitor's 7-bit I²C address for DDC/CI.
pub const ADDRESS: u16 = 0x37;
const HOST: u8 = 0x51;
const DEST_WRITE: u8 = 0x6E;
const HOST_READ: u8 = 0x50;
const GET_VCP: u8 = 0x01;
const GET_VCP_REPLY: u8 = 0x02;
const SET_VCP: u8 = 0x03;
/// How long a monitor may take to prepare a reply.
const REPLY_WAIT: Duration = Duration::from_millis(50);
/// How long to leave a monitor alone after setting a value.
const SET_WAIT: Duration = Duration::from_millis(50);
const TRIES: usize = 3;

fn checksum(start: u8, bytes: &[u8]) -> u8 {
    bytes.iter().fold(start, |acc, b| acc ^ b)
}

/// A host-to-monitor message carrying `data`.
pub fn message(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 3);
    out.push(HOST);
    out.push(0x80 | data.len() as u8);
    out.extend_from_slice(data);
    out.push(checksum(DEST_WRITE, &out));
    out
}

pub fn get_vcp(code: u8) -> Vec<u8> {
    message(&[GET_VCP, code])
}

pub fn set_vcp(code: u8, value: u16) -> Vec<u8> {
    let [hi, lo] = value.to_be_bytes();
    message(&[SET_VCP, code, hi, lo])
}

/// A control's maximum and current value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub max: u16,
    pub value: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyError {
    /// The monitor sent nothing useful yet (a null message or a short or
    /// corrupt reply): worth asking again.
    Retry,
    /// The monitor does not have this control.
    Unsupported,
    /// A reply for something else.
    Unexpected,
}

/// Parse the reply to [`get_vcp`] for `code`.
pub fn parse_vcp_reply(code: u8, reply: &[u8]) -> Result<Reading, ReplyError> {
    if reply.len() < 3 || reply[0] != DEST_WRITE {
        return Err(ReplyError::Retry);
    }
    let len = usize::from(reply[1] & 0x7F);
    if reply[1] & 0x80 == 0 || reply.len() < len + 3 {
        return Err(ReplyError::Retry);
    }
    let body = &reply[..len + 2];
    if checksum(HOST_READ, body) != reply[len + 2] {
        return Err(ReplyError::Retry);
    }
    let data = &body[2..];
    if data.is_empty() {
        return Err(ReplyError::Retry);
    }
    if data.len() != 8 || data[0] != GET_VCP_REPLY || data[2] != code {
        return Err(ReplyError::Unexpected);
    }
    if data[1] != 0 {
        return Err(ReplyError::Unsupported);
    }
    Ok(Reading {
        max: u16::from_be_bytes([data[4], data[5]]),
        value: u16::from_be_bytes([data[6], data[7]]),
    })
}

/// An I²C bus with the monitor's DDC/CI address selected.
pub trait Bus {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn read(&mut self, buf: &mut [u8]) -> io::Result<()>;
    fn pause(&mut self, time: Duration);
}

/// Read `code` from the monitor.
pub fn read_vcp(bus: &mut dyn Bus, code: u8) -> io::Result<Reading> {
    for _ in 0..TRIES {
        bus.write(&get_vcp(code))?;
        bus.pause(REPLY_WAIT);
        let mut reply = [0u8; 11];
        bus.read(&mut reply)?;
        match parse_vcp_reply(code, &reply) {
            Ok(reading) => return Ok(reading),
            Err(ReplyError::Unsupported) => {
                return Err(io::Error::new(io::ErrorKind::Unsupported, format!("no VCP control {code:#04x}")))
            }
            Err(ReplyError::Retry | ReplyError::Unexpected) => bus.pause(REPLY_WAIT),
        }
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "no valid DDC/CI reply"))
}

/// Set `code` to `value`.
pub fn write_vcp(bus: &mut dyn Bus, code: u8, value: u16) -> io::Result<()> {
    bus.write(&set_vcp(code, value))?;
    bus.pause(SET_WAIT);
    Ok(())
}

/// A `/dev/i2c-N` device.
pub struct I2cDev(std::fs::File);

impl I2cDev {
    pub fn open(path: &Path) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        const I2C_SLAVE: libc::c_ulong = 0x0703;
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        // SAFETY: I2C_SLAVE takes the address as a plain integer argument.
        if unsafe { libc::ioctl(file.as_raw_fd(), I2C_SLAVE as _, libc::c_ulong::from(ADDRESS)) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(file))
    }
}

impl Bus for I2cDev {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.write_all(bytes)
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.0.read_exact(buf)
    }

    fn pause(&mut self, time: Duration) {
        std::thread::sleep(time);
    }
}

/// The I²C device for `connector` (such as `DP-1`) under `drm_root`
/// (`/sys/class/drm`): the connector's `ddc` link, or an `i2c-N` entry in
/// its directory. The device node is looked up under `dev_root` (`/dev`).
pub fn find_bus(drm_root: &Path, dev_root: &Path, connector: &str) -> Option<PathBuf> {
    let suffix = format!("-{connector}");
    let dir = std::fs::read_dir(drm_root)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("card") && n.ends_with(&suffix) && !n[4..n.len() - suffix.len()].contains('-'))
        })?;
    let bus_name = |p: &Path| p.file_name().and_then(|n| n.to_str()).filter(|n| n.starts_with("i2c-")).map(str::to_owned);
    let name = std::fs::read_link(dir.join("ddc"))
        .ok()
        .and_then(|target| bus_name(&target))
        .or_else(|| {
            std::fs::read_dir(&dir).ok()?.filter_map(Result::ok).find_map(|e| bus_name(&e.path()))
        })?;
    Some(dev_root.join(name))
}

/// What is known about one connector's monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Probe {
    /// Brightness is controllable through this bus.
    Found { bus: PathBuf, reading: Reading },
    /// No DDC/CI brightness (no bus, no answer or no such control).
    Missing,
}

/// Opens the bus for a device path; replaced in tests.
pub type Opener = Box<dyn Fn(&Path) -> io::Result<Box<dyn Bus>> + Send + Sync>;

/// External monitors with DDC/CI brightness, by connector.
pub struct Ddc {
    drm_root: PathBuf,
    dev_root: PathBuf,
    open: Opener,
    known: Mutex<HashMap<String, Probe>>,
}

impl std::fmt::Debug for Ddc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ddc").field("drm_root", &self.drm_root).field("known", &self.known).finish()
    }
}

impl Ddc {
    pub fn new(drm_root: PathBuf, dev_root: PathBuf, open: Opener) -> Self {
        Self { drm_root, dev_root, open, known: Mutex::default() }
    }

    /// `/sys/class/drm` and `/dev`, through real I²C devices.
    pub fn system() -> Self {
        Self::new(
            PathBuf::from("/sys/class/drm"),
            PathBuf::from("/dev"),
            Box::new(|path| I2cDev::open(path).map(|d| Box::new(d) as Box<dyn Bus>)),
        )
    }

    fn known(&self) -> std::sync::MutexGuard<'_, HashMap<String, Probe>> {
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Follow the current external connectors: forget the ones gone, and
    /// ask new ones (blocking; up to a few hundred milliseconds each).
    /// With `reread`, the brightness of known monitors is read again.
    /// Returns whether anything changed.
    pub fn refresh(&self, connectors: &[String], reread: bool) -> bool {
        let mut changed = false;
        {
            let mut known = self.known();
            let before = known.len();
            known.retain(|c, _| connectors.contains(c));
            changed |= known.len() != before;
        }
        for connector in connectors {
            let previous = self.known().get(connector).cloned();
            let probe = match previous {
                Some(Probe::Missing) => continue,
                Some(Probe::Found { bus, reading }) if reread => match self.read(&bus) {
                    Ok(now) => Probe::Found { bus, reading: now },
                    Err(err) => {
                        tracing::debug!(connector, %err, "DDC/CI brightness not readable");
                        Probe::Found { bus, reading }
                    }
                },
                Some(found) => found,
                None => match find_bus(&self.drm_root, &self.dev_root, connector) {
                    Some(bus) => match self.read(&bus) {
                        Ok(reading) if reading.max > 0 => {
                            tracing::info!(connector, bus = %bus.display(), max = reading.max, "DDC/CI brightness");
                            Probe::Found { bus, reading }
                        }
                        Ok(_) => Probe::Missing,
                        Err(err) => {
                            tracing::debug!(connector, %err, "no DDC/CI brightness");
                            Probe::Missing
                        }
                    },
                    None => Probe::Missing,
                },
            };
            let mut known = self.known();
            if known.get(connector) != Some(&probe) {
                changed = true;
                known.insert(connector.clone(), probe);
            }
        }
        changed
    }

    fn read(&self, bus: &Path) -> io::Result<Reading> {
        let mut dev = (self.open)(bus)?;
        read_vcp(dev.as_mut(), BRIGHTNESS)
    }

    /// The last reading for `connector`, if its brightness is controllable.
    pub fn reading(&self, connector: &str) -> Option<Reading> {
        match self.known().get(connector)? {
            Probe::Found { reading, .. } => Some(*reading),
            Probe::Missing => None,
        }
    }

    /// Set `connector`'s brightness (blocking).
    pub fn set(&self, connector: &str, value: u16) -> io::Result<()> {
        let Some(Probe::Found { bus, reading }) = self.known().get(connector).cloned() else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no DDC/CI brightness"));
        };
        if value > reading.max {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "brightness out of range"));
        }
        let mut dev = (self.open)(&bus)?;
        write_vcp(dev.as_mut(), BRIGHTNESS, value)?;
        if let Some(Probe::Found { reading, .. }) = self.known().get_mut(connector) {
            reading.value = value;
        }
        Ok(())
    }
}

/// A simulated monitor answering DDC/CI as the standard describes, for
/// tests (no real monitor was available while writing this).
#[doc(hidden)]
pub mod simulated {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A monitor answering DDC/CI as the standard describes.
    #[derive(Debug, Default)]
    pub struct Monitor {
        pub brightness: Option<(u16, u16)>,
        /// Replies to corrupt before answering properly.
        pub garble: usize,
        pub pending: Option<Vec<u8>>,
        pub writes: Vec<Vec<u8>>,
    }

    pub fn reply(data: &[u8]) -> Vec<u8> {
        let mut out = vec![DEST_WRITE, 0x80 | data.len() as u8];
        out.extend_from_slice(data);
        out.push(checksum(HOST_READ, &out));
        out
    }

    pub struct Fake(pub Arc<Mutex<Monitor>>);

    impl Bus for Fake {
        fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            let mut m = self.0.lock().unwrap();
            m.writes.push(bytes.to_vec());
            assert_eq!(bytes[0], HOST);
            let n = usize::from(bytes[1] & 0x7F);
            assert_eq!(bytes.len(), n + 3);
            assert_eq!(checksum(DEST_WRITE, &bytes[..n + 2]), bytes[n + 2], "host checksum");
            let data = &bytes[2..n + 2];
            match (data[0], m.brightness) {
                (GET_VCP, _) if m.garble > 0 => {
                    m.garble -= 1;
                    m.pending = Some(vec![DEST_WRITE, 0x80, checksum(HOST_READ, &[DEST_WRITE, 0x80])]);
                }
                (GET_VCP, Some((max, cur))) if data[1] == BRIGHTNESS => {
                    let [mh, ml] = max.to_be_bytes();
                    let [ch, cl] = cur.to_be_bytes();
                    m.pending = Some(reply(&[GET_VCP_REPLY, 0, data[1], 0, mh, ml, ch, cl]));
                }
                (GET_VCP, _) => m.pending = Some(reply(&[GET_VCP_REPLY, 1, data[1], 0, 0, 0, 0, 0])),
                (SET_VCP, Some((max, _))) if data[1] == BRIGHTNESS => {
                    m.brightness = Some((max, u16::from_be_bytes([data[2], data[3]])));
                }
                _ => {}
            }
            Ok(())
        }

        fn read(&mut self, buf: &mut [u8]) -> io::Result<()> {
            let pending = self.0.lock().unwrap().pending.take().ok_or(io::ErrorKind::TimedOut)?;
            buf.fill(0);
            let n = pending.len().min(buf.len());
            buf[..n].copy_from_slice(&pending[..n]);
            Ok(())
        }

        fn pause(&mut self, _: Duration) {}
    }
}

#[cfg(test)]
mod tests {
    use super::simulated::{reply, Fake, Monitor};
    use super::*;
    use std::sync::Arc;

    #[test]
    fn messages() {
        // Get VCP 0x10: 0x6E ^ 0x51 ^ 0x82 ^ 0x01 ^ 0x10 = 0xAC.
        assert_eq!(get_vcp(BRIGHTNESS), [0x51, 0x82, 0x01, 0x10, 0xAC]);
        assert_eq!(set_vcp(BRIGHTNESS, 0x0032), [0x51, 0x84, 0x03, 0x10, 0x00, 0x32, 0x6E ^ 0x51 ^ 0x84 ^ 0x03 ^ 0x10 ^ 0x32]);
    }

    #[test]
    fn replies() {
        let ok = reply(&[GET_VCP_REPLY, 0, 0x10, 0, 0, 100, 0, 42]);
        assert_eq!(parse_vcp_reply(0x10, &ok), Ok(Reading { max: 100, value: 42 }));
        let mut bad = ok.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(parse_vcp_reply(0x10, &bad), Err(ReplyError::Retry));
        assert_eq!(parse_vcp_reply(0x12, &ok), Err(ReplyError::Unexpected));
        let unsupported = reply(&[GET_VCP_REPLY, 1, 0x10, 0, 0, 0, 0, 0]);
        assert_eq!(parse_vcp_reply(0x10, &unsupported), Err(ReplyError::Unsupported));
        assert_eq!(parse_vcp_reply(0x10, &[0x6E, 0x80, 0x6E ^ 0x80 ^ 0x50]), Err(ReplyError::Retry), "null message");
        assert_eq!(parse_vcp_reply(0x10, &[0; 11]), Err(ReplyError::Retry));
    }

    #[test]
    fn transactions() {
        let monitor = Arc::new(Mutex::new(Monitor { brightness: Some((100, 70)), garble: 2, ..Default::default() }));
        let mut bus = Fake(monitor.clone());
        assert_eq!(read_vcp(&mut bus, BRIGHTNESS).unwrap(), Reading { max: 100, value: 70 }, "retried past null replies");
        write_vcp(&mut bus, BRIGHTNESS, 30).unwrap();
        assert_eq!(read_vcp(&mut bus, BRIGHTNESS).unwrap().value, 30);
        assert_eq!(read_vcp(&mut bus, 0x12).unwrap_err().kind(), io::ErrorKind::Unsupported);
        monitor.lock().unwrap().garble = 5;
        assert!(read_vcp(&mut bus, BRIGHTNESS).is_err(), "gives up");
    }

    fn scratch() -> PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("helix-ddc-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sysfs(root: &Path) -> (PathBuf, PathBuf) {
        let drm = root.join("drm");
        let dev = root.join("dev");
        std::fs::create_dir_all(drm.join("card1-DP-1")).unwrap();
        std::os::unix::fs::symlink("../../../i2c-7", drm.join("card1-DP-1/ddc")).unwrap();
        std::fs::create_dir_all(drm.join("card1-HDMI-A-1/i2c-9")).unwrap();
        std::fs::create_dir_all(drm.join("card1-DP-11")).unwrap();
        std::fs::create_dir_all(drm.join("card0-Unknown-1")).unwrap();
        (drm, dev)
    }

    #[test]
    fn buses() {
        let dir = scratch();
        let (drm, dev) = sysfs(&dir);
        assert_eq!(find_bus(&drm, &dev, "DP-1"), Some(dev.join("i2c-7")));
        assert_eq!(find_bus(&drm, &dev, "HDMI-A-1"), Some(dev.join("i2c-9")));
        assert_eq!(find_bus(&drm, &dev, "DP-11"), None, "no bus");
        assert_eq!(find_bus(&drm, &dev, "1"), None, "card1-DP-1 is not connector 1");
        assert_eq!(find_bus(&drm, &dev, "DP-2"), None);
    }

    #[test]
    fn monitors() {
        let dir = scratch();
        let (drm, dev) = sysfs(&dir);
        let dp = Arc::new(Mutex::new(Monitor { brightness: Some((100, 80)), ..Default::default() }));
        let hdmi = Arc::new(Mutex::new(Monitor::default()));
        let opens = Arc::new(Mutex::new(Vec::new()));
        let (dp2, hdmi2, opens2, dev2) = (dp.clone(), hdmi.clone(), opens.clone(), dev.clone());
        let ddc = Ddc::new(
            drm,
            dev,
            Box::new(move |path| {
                opens2.lock().unwrap().push(path.to_owned());
                let monitor = if path == dev2.join("i2c-7") { dp2.clone() } else { hdmi2.clone() };
                Ok(Box::new(Fake(monitor)) as Box<dyn Bus>)
            }),
        );
        let connectors = vec!["DP-1".to_owned(), "HDMI-A-1".to_owned()];
        assert!(ddc.refresh(&connectors, false));
        assert_eq!(ddc.reading("DP-1"), Some(Reading { max: 100, value: 80 }));
        assert_eq!(ddc.reading("HDMI-A-1"), None, "no brightness control");
        assert_eq!(opens.lock().unwrap().len(), 2);
        assert!(!ddc.refresh(&connectors, false), "nothing asked again");
        assert_eq!(opens.lock().unwrap().len(), 2);

        ddc.set("DP-1", 25).unwrap();
        assert_eq!(dp.lock().unwrap().brightness, Some((100, 25)));
        assert_eq!(ddc.reading("DP-1").unwrap().value, 25);
        assert!(ddc.set("DP-1", 101).is_err());
        assert!(ddc.set("HDMI-A-1", 5).is_err());

        dp.lock().unwrap().brightness = Some((100, 60));
        assert!(ddc.refresh(&connectors, true), "changed on the monitor");
        assert_eq!(ddc.reading("DP-1").unwrap().value, 60);

        assert!(ddc.refresh(&["HDMI-A-1".to_owned()], true), "DP-1 unplugged");
        assert_eq!(ddc.reading("DP-1"), None);
    }
}
