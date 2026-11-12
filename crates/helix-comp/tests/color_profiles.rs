//! Monitors as colord devices, and their profiles' calibration curves
//! (helix-desktop-spec `org.gnome.Mutter.DisplayConfig.md`, "Color
//! profiles"), against a fake colord on a private bus. No real monitor or
//! profile is involved.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_comp::color_profiles::{self, COLORD, COLORD_PATH};
use helix_comp::display_config::{identity_ramps, ColorRequest, ColorSet, Mode, Monitor, Ramps, ALL_TRANSFORMS};
use helix_test_support::TestBus;
use tokio::sync::mpsc;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedObjectPath;
use zbus::{interface, Connection};

#[derive(Default)]
struct Colord {
    calls: Vec<String>,
    /// Device id → its profile's file ("" for none).
    profiles: HashMap<String, String>,
}

type Shared = Arc<Mutex<Colord>>;

struct Manager(Shared);

fn device_path(id: &str) -> String {
    format!("/org/freedesktop/ColorManager/devices/{}", id.replace(|c: char| !c.is_ascii_alphanumeric(), "_"))
}

#[interface(name = "org.freedesktop.ColorManager")]
impl Manager {
    async fn create_device(&self, id: String, scope: String, props: HashMap<String, String>, #[zbus(connection)] conn: &Connection) -> zbus::fdo::Result<OwnedObjectPath> {
        let mut keys: Vec<String> = props.iter().map(|(k, v)| format!("{k}={v}")).collect();
        keys.sort();
        self.0.lock().unwrap().calls.push(format!("CreateDevice {id} {scope} {}", keys.join(",")));
        let path = device_path(&id);
        conn.object_server().at(path.as_str(), Device { id, shared: self.0.clone() }).await?;
        Ok(OwnedObjectPath::try_from(path).unwrap())
    }

    async fn delete_device(&self, path: OwnedObjectPath) {
        self.0.lock().unwrap().calls.push(format!("DeleteDevice {}", path.as_str()));
    }

    #[zbus(signal)]
    async fn device_changed(emitter: &SignalEmitter<'_>, path: OwnedObjectPath) -> zbus::Result<()>;
}

struct Device {
    id: String,
    shared: Shared,
}

#[interface(name = "org.freedesktop.ColorManager.Device")]
impl Device {
    #[zbus(property)]
    fn profiles(&self) -> Vec<OwnedObjectPath> {
        match self.shared.lock().unwrap().profiles.get(&self.id) {
            Some(file) if !file.is_empty() => vec![OwnedObjectPath::try_from(format!("{}_profile", device_path(&self.id))).unwrap()],
            _ => Vec::new(),
        }
    }
}

struct Profile(String);

#[interface(name = "org.freedesktop.ColorManager.Profile")]
impl Profile {
    #[zbus(property)]
    fn filename(&self) -> String {
        self.0.clone()
    }
}

fn monitor(connector: &str, hdr_mode: u32) -> Monitor {
    Monitor {
        connector: connector.into(),
        vendor: "DEL".into(),
        product: "U2720Q".into(),
        serial: connector.into(),
        display_name: connector.into(),
        builtin: false,
        modes: vec![Mode { width: 1920, height: 1080, refresh_mhz: 60_000, preferred: true, current: true }],
        x: 0,
        y: 0,
        scale: 1.0,
        transform: 0,
        primary: true,
        transforms: ALL_TRANSFORMS.to_vec(),
        hdr: true,
        color_mode: hdr_mode,
        underscan: None,
    }
}

/// A profile with a `vcgt` table halving every channel.
fn halving_profile(path: &std::path::Path) {
    let mut tag = b"vcgt".to_vec();
    tag.extend([0, 0, 0, 0]);
    tag.extend(0u32.to_be_bytes());
    tag.extend(3u16.to_be_bytes());
    tag.extend(2u16.to_be_bytes());
    tag.extend(2u16.to_be_bytes());
    for v in [0u16, 32768, 0, 32768, 0, 32768] {
        tag.extend(v.to_be_bytes());
    }
    let mut icc = vec![0u8; 144];
    icc[36..40].copy_from_slice(b"acsp");
    icc[128..132].copy_from_slice(&1u32.to_be_bytes());
    icc[132..136].copy_from_slice(b"vcgt");
    icc[136..140].copy_from_slice(&144u32.to_be_bytes());
    icc[140..144].copy_from_slice(&(tag.len() as u32).to_be_bytes());
    icc.extend(tag);
    std::fs::write(path, icc).unwrap();
}

/// A calibration the compositor was given: connector and curves.
type Calibration = (String, Option<Ramps>);

#[tokio::test(flavor = "multi_thread")]
async fn devices_and_calibration() {
    let Some(bus) = TestBus::start() else { return };
    let shared = Shared::default();
    let colord = bus.builder().name(COLORD).unwrap().serve_at(COLORD_PATH, Manager(shared.clone())).unwrap().build().await.unwrap();
    let file = std::env::temp_dir().join(format!("helix-profile-{}.icc", std::process::id()));
    halving_profile(&file);

    // The compositor: four-entry ramps; records calibrations.
    let (gamma, mut requests) = mpsc::unbounded_channel::<ColorRequest>();
    let calibrations: Arc<Mutex<Vec<Calibration>>> = Default::default();
    let seen = calibrations.clone();
    tokio::spawn(async move {
        while let Some(ColorRequest { connector, set, reply }) = requests.recv().await {
            if let Some(ColorSet::Calibration(curves)) = set {
                seen.lock().unwrap().push((connector, curves));
            }
            let _ = reply.send(Ok(identity_ramps(4)));
        }
    });
    let (monitors, rx) = mpsc::unbounded_channel();
    tokio::spawn(color_profiles::run(bus.connect().await, gamma, rx));

    monitors.send(vec![monitor("DP-1", 0)]).unwrap();
    let wait = || async {
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let until = |f: &dyn Fn() -> bool| {
        let ok = (0..200).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            f()
        });
        assert!(ok, "condition never became true");
    };
    until(&|| !calibrations.lock().unwrap().is_empty());
    assert_eq!(
        shared.lock().unwrap().calls,
        ["CreateDevice xrandr-DEL-U2720Q-DP-1 temp Colorspace=rgb,Kind=display,Mode=physical,Model=U2720Q,Serial=DP-1,Vendor=DEL,XRANDR_name=DP-1"]
    );
    assert_eq!(calibrations.lock().unwrap()[0], ("DP-1".into(), None), "no profile yet");

    // A profile is assigned: its curves are applied.
    let id = "xrandr-DEL-U2720Q-DP-1";
    colord.object_server().at(format!("{}_profile", device_path(id)), Profile(file.to_string_lossy().into_owned())).await.unwrap();
    shared.lock().unwrap().profiles.insert(id.into(), file.to_string_lossy().into_owned());
    let path = OwnedObjectPath::try_from(device_path(id)).unwrap();
    let emitter = SignalEmitter::new(&colord, COLORD_PATH).unwrap();
    Manager::device_changed(&emitter, path).await.unwrap();
    until(&|| calibrations.lock().unwrap().len() >= 2);
    let (connector, curves) = calibrations.lock().unwrap()[1].clone();
    assert_eq!(connector, "DP-1");
    assert_eq!(curves.unwrap().0, [0, 10923, 21845, 32768], "halved, resampled to the ramp size");

    // In the BT.2100 color mode the curves are not applied.
    monitors.send(vec![monitor("DP-1", 1)]).unwrap();
    until(&|| calibrations.lock().unwrap().len() >= 3);
    assert_eq!(calibrations.lock().unwrap()[2], ("DP-1".into(), None));

    // The monitor goes: so does its device.
    monitors.send(Vec::new()).unwrap();
    wait().await;
    assert_eq!(shared.lock().unwrap().calls.last().unwrap(), &format!("DeleteDevice {}", device_path(id)));
    let _ = std::fs::remove_file(&file);
}
