//! DisplayConfig `GetCrtcGamma` / `SetCrtcGamma` / `SetOutputCTM` over a
//! private bus, with a stand-in compositor that keeps ramps of 4 entries
//! per monitor and matrices.

use std::collections::HashMap;

use helix_comp::display_config::{identity_ramps, ColorRequest, ColorSet, Ctm, Mode, Monitor, Ramps, Store, ALL_TRANSFORMS, IDENTITY_CTM};
use helix_comp::{CompositorServices, MonitorControl};
use helix_test_support::TestBus;
use tokio::sync::mpsc;

#[zbus::proxy(
    interface = "org.gnome.Mutter.DisplayConfig",
    default_service = "org.gnome.Mutter.DisplayConfig",
    default_path = "/org/gnome/Mutter/DisplayConfig"
)]
trait DisplayConfig {
    #[allow(clippy::type_complexity)]
    fn get_resources(
        &self,
    ) -> zbus::Result<(
        u32,
        Vec<(u32, i64, i32, i32, i32, i32, i32, u32, Vec<u32>, HashMap<String, zbus::zvariant::OwnedValue>)>,
        Vec<(u32, i64, i32, Vec<u32>, String, Vec<u32>, Vec<u32>, HashMap<String, zbus::zvariant::OwnedValue>)>,
        Vec<(u32, i64, u32, u32, f64, u32)>,
        i32,
        i32,
    )>;
    fn get_crtc_gamma(&self, serial: u32, crtc: u32) -> zbus::Result<(Vec<u16>, Vec<u16>, Vec<u16>)>;
    fn set_crtc_gamma(&self, serial: u32, crtc: u32, red: &[u16], green: &[u16], blue: &[u16]) -> zbus::Result<()>;
    #[zbus(name = "SetOutputCTM")]
    fn set_output_ctm(&self, serial: u32, output: u32, ctm: &(u64, u64, u64, u64, u64, u64, u64, u64, u64)) -> zbus::Result<()>;
}

fn monitor(connector: &str, x: i32) -> Monitor {
    Monitor {
        connector: connector.into(),
        vendor: "Acme".into(),
        product: "Panel".into(),
        serial: connector.into(),
        display_name: connector.into(),
        builtin: false,
        modes: vec![Mode { width: 1920, height: 1080, refresh_mhz: 60_000, preferred: true, current: true }],
        x,
        y: 0,
        scale: 1.0,
        transform: 0,
        primary: x == 0,
        transforms: ALL_TRANSFORMS.to_vec(),
        hdr: false,
        color_mode: 0,
    }
}

/// The stand-in compositor and the matrices it was given.
fn fake_compositor() -> (mpsc::UnboundedSender<ColorRequest>, mpsc::UnboundedReceiver<(String, Ctm)>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<ColorRequest>();
    let (ctm_tx, ctm_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut kept: HashMap<String, Ramps> = HashMap::new();
        while let Some(ColorRequest { connector, set, reply }) = rx.recv().await {
            let current = kept.get(&connector).cloned().unwrap_or_else(|| identity_ramps(4));
            let result = match set {
                None => Ok(current),
                Some(ColorSet::Gamma(r)) if r.0.len() != 4 || r.1.len() != 4 || r.2.len() != 4 => {
                    Err(helix_comp::display_config::ColorError::Invalid("ramps must have 4 entries".into()))
                }
                Some(ColorSet::Gamma(r)) => {
                    kept.insert(connector, r.clone());
                    Ok(r)
                }
                Some(ColorSet::Ctm(ctm)) => {
                    let _ = ctm_tx.send((connector, ctm));
                    Ok(current)
                }
                Some(ColorSet::PowerSave(_) | ColorSet::Grayscale(_) | ColorSet::Calibration(_) | ColorSet::ClickType(_)) => Ok(current),
            };
            let _ = reply.send(result);
        }
    });
    (tx, ctm_rx)
}

#[tokio::test]
async fn gamma_per_crtc() {
    let Some(bus) = TestBus::start() else { return };
    let store = std::env::temp_dir().join(format!("helix-gamma-{}.toml", std::process::id()));
    let (compositor, mut matrices) = fake_compositor();
    let control = MonitorControl { store: Store::at(store), gamma: Some(compositor), ..MonitorControl::default() };
    let services = CompositorServices::start_with(bus.builder(), vec![monitor("DP-1", 0), monitor("HDMI-1", 1920)], control)
        .await
        .unwrap();
    let (_events, rx) = mpsc::unbounded_channel();
    tokio::spawn(services.run(rx));
    let conn = bus.connect().await;
    let dc = DisplayConfigProxy::new(&conn).await.unwrap();
    let (serial, crtcs, ..) = dc.get_resources().await.unwrap();
    assert_eq!(crtcs.len(), 2);

    let (r, g, b) = dc.get_crtc_gamma(serial, 1).await.unwrap();
    assert_eq!(r, [0, 21845, 43690, 65535], "identity at first");
    assert_eq!((g.len(), b.len()), (4, 4));
    let warm = [0u16, 20000, 40000, 60000];
    dc.set_crtc_gamma(serial, 1, &[0, 1, 2, 3], &warm, &warm).await.unwrap();
    assert_eq!(dc.get_crtc_gamma(serial, 1).await.unwrap().0, [0, 1, 2, 3]);
    assert_eq!(dc.get_crtc_gamma(serial, 0).await.unwrap().0, [0, 21845, 43690, 65535], "other CRTC untouched");

    // Refused: stale serial, unknown CRTC, wrong length.
    assert!(dc.get_crtc_gamma(serial + 1, 0).await.is_err());
    assert!(dc.get_crtc_gamma(serial, 2).await.is_err());
    assert!(dc.set_crtc_gamma(serial, 0, &[0, 1], &[0, 1], &[0, 1]).await.is_err());

    // A matrix: half red (0.5 = 1 << 31), the rest as is.
    let one = IDENTITY_CTM[0];
    let ctm = (1u64 << 31, 0, 0, 0, one, 0, 0, 0, one);
    dc.set_output_ctm(serial, 1, &ctm).await.unwrap();
    let (connector, got) = matrices.recv().await.unwrap();
    assert_eq!(connector, "HDMI-1");
    assert_eq!(got, [1 << 31, 0, 0, 0, one, 0, 0, 0, one]);
    assert!(dc.set_output_ctm(serial + 1, 1, &ctm).await.is_err(), "stale serial");
    assert!(dc.set_output_ctm(serial, 2, &ctm).await.is_err(), "unknown output");
}
