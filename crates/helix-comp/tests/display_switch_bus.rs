//! The display switcher against a fake compositor (helix-desktop-spec
//! `org.gnome.Mutter.DisplayConfig.md`, "Display switcher").

use std::time::Duration;

use helix_comp::display_config::{ApplyRequest, Layout, Mode, Monitor, Store, ALL_TRANSFORMS};
use helix_comp::{CompositorServices, MonitorControl};
use helix_test_support::TestBus;
use tokio::sync::mpsc;

fn monitor(connector: &str, x: i32, builtin: bool) -> Monitor {
    Monitor {
        connector: connector.into(),
        vendor: "Acme".into(),
        product: "Panel".into(),
        serial: connector.into(),
        display_name: connector.into(),
        builtin,
        modes: vec![Mode { width: 1920, height: 1080, refresh_mhz: 60_000, preferred: true, current: true }],
        x,
        y: 0,
        scale: 1.0,
        transform: 0,
        primary: builtin,
        transforms: ALL_TRANSFORMS.to_vec(),
        hdr: false,
        color_mode: 0,
        underscan: None,
    }
}

fn fake_compositor(initial: Vec<Monitor>) -> mpsc::UnboundedSender<ApplyRequest> {
    let (tx, mut rx) = mpsc::unbounded_channel::<ApplyRequest>();
    tokio::spawn(async move {
        let mut current = initial;
        while let Some(request) = rx.recv().await {
            current = Layout::new(current).applied(&request.config);
            let _ = request.reply.send(Ok(current.clone()));
        }
    });
    tx
}

async fn logical(services: &CompositorServices) -> Vec<Vec<String>> {
    let reply = services
        .connection()
        .call_method(Some("org.gnome.Mutter.DisplayConfig"), "/org/gnome/Mutter/DisplayConfig", Some("org.gnome.Mutter.DisplayConfig"), "GetCurrentState", &())
        .await
        .unwrap();
    type Props = std::collections::HashMap<String, zbus::zvariant::OwnedValue>;
    type Spec = (String, String, String, String);
    type Logical = (i32, i32, f64, u32, bool, Vec<Spec>, Props);
    type WireMonitor = (Spec, Vec<(String, i32, i32, f64, f64, Vec<f64>, Props)>, Props);
    let (_, _, logical, _): (u32, Vec<WireMonitor>, Vec<Logical>, Props) = reply.body().deserialize().unwrap();
    let mut out: Vec<Vec<String>> = logical.into_iter().map(|l| l.5.into_iter().map(|s| s.0).collect()).collect();
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn presses_settle_on_a_mode() {
    let Some(bus) = TestBus::start() else { return };
    let dir = std::env::temp_dir().join(format!("helix-switch-{}", std::process::id()));
    let monitors = vec![monitor("eDP-1", 0, true), monitor("HDMI-1", 1920, false)];
    let control = MonitorControl { applier: Some(fake_compositor(monitors.clone())), store: Store::at(dir.join("monitors.toml")), ..MonitorControl::default() };
    let services = CompositorServices::start_with(bus.builder(), monitors, control).await.unwrap();
    assert_eq!(logical(&services).await, [vec!["HDMI-1".to_owned()], vec!["eDP-1".to_owned()]], "joined");

    // Two quick presses: External Only, then Built-in Only.
    services.handle(helix_comp::Event::SwitchMonitor).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    services.handle(helix_comp::Event::SwitchMonitor).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(logical(&services).await.len(), 2, "nothing applied while pressing");
    tokio::time::sleep(helix_comp::display_switch::SETTLE).await;
    assert_eq!(logical(&services).await, [vec!["eDP-1".to_owned()]]);

    // Then Mirror.
    services.handle(helix_comp::Event::SwitchMonitor).await.unwrap();
    tokio::time::sleep(helix_comp::display_switch::SETTLE + Duration::from_millis(500)).await;
    assert_eq!(logical(&services).await, [vec!["eDP-1".to_owned(), "HDMI-1".to_owned()]], "mirrored");
    let _ = std::fs::remove_dir_all(&dir);
}
