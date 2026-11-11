//! DisplayConfig `GetCrtcGamma` / `SetCrtcGamma` over a private bus, with a
//! stand-in compositor that keeps ramps of 4 entries per monitor.

use std::collections::HashMap;

use helix_comp::display_config::{identity_ramps, GammaRequest, Mode, Monitor, Ramps, Store, ALL_TRANSFORMS};
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
    }
}

fn fake_compositor() -> mpsc::UnboundedSender<GammaRequest> {
    let (tx, mut rx) = mpsc::unbounded_channel::<GammaRequest>();
    tokio::spawn(async move {
        let mut kept: HashMap<String, Ramps> = HashMap::new();
        while let Some(GammaRequest { connector, set, reply }) = rx.recv().await {
            let result = match set {
                None => Ok(kept.get(&connector).cloned().unwrap_or_else(|| identity_ramps(4))),
                Some(r) if r.0.len() != 4 || r.1.len() != 4 || r.2.len() != 4 => Err("ramps must have 4 entries".into()),
                Some(r) => {
                    kept.insert(connector, r.clone());
                    Ok(r)
                }
            };
            let _ = reply.send(result);
        }
    });
    tx
}

#[tokio::test]
async fn gamma_per_crtc() {
    let Some(bus) = TestBus::start() else { return };
    let store = std::env::temp_dir().join(format!("helix-gamma-{}.toml", std::process::id()));
    let control = MonitorControl { store: Store::at(store), gamma: Some(fake_compositor()), ..MonitorControl::default() };
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
}
