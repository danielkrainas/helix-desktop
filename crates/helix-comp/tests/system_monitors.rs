//! Remembered configurations from the system-wide file (helix-desktop-spec
//! `org.gnome.Mutter.DisplayConfig.md`, "Remembered configurations"),
//! against a fake compositor: the greeter's case, where the greeter user
//! has no layout of its own.

use helix_comp::display_config::{ApplyRequest, Layout, Mode, Monitor, MonitorChoice, MonitorsConfig, LogicalConfig, Store, ALL_TRANSFORMS};
use helix_comp::{CompositorServices, MonitorControl};
use helix_test_support::TestBus;
use tokio::sync::mpsc;

/// The first connector is primary, as the compositor starts.
fn monitor(connector: &str, x: i32) -> Monitor {
    Monitor {
        connector: connector.into(),
        vendor: "Acme".into(),
        product: "Screen".into(),
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

/// `primary` on the left, `other` on its right.
fn primary_left(primary: &str, other: &str) -> MonitorsConfig {
    let place = |x, primary, connector: &str| LogicalConfig {
        x,
        y: 0,
        scale: 1.0,
        transform: 0,
        primary,
        monitors: vec![MonitorChoice { connector: connector.into(), width: 1920, height: 1080, refresh_mhz: 60_000, color_mode: 0, underscan: false }],
    };
    MonitorsConfig { logical: vec![place(0, true, primary), place(1920, false, other)] }
}

async fn primary(services: &CompositorServices) -> String {
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
    let primary = logical.into_iter().find(|l| l.4).expect("a primary");
    primary.5[0].0.clone()
}

/// Start the compositor's services with `store`, restoring what it holds.
async fn start(bus: &TestBus, store: Store) -> CompositorServices {
    let monitors = vec![monitor("DVI-D-1", 0), monitor("DP-3", 1920)];
    let control = MonitorControl { applier: Some(fake_compositor(monitors.clone())), store, ..MonitorControl::default() };
    let services = CompositorServices::start_with(bus.builder(), monitors, control).await.unwrap();
    services.restore_saved().await;
    services
}

#[tokio::test(flavor = "multi_thread")]
async fn system_layout_when_the_user_has_none() {
    let dir = std::env::temp_dir().join(format!("helix-system-monitors-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let user = dir.join("home/.config/helix/monitors.toml");
    let system = dir.join("etc/xdg/helix/monitors.toml");
    let key = Layout::new(vec![monitor("DVI-D-1", 0), monitor("DP-3", 1920)]).key();
    Store::at(system.clone()).save(&key, &primary_left("DP-3", "DVI-D-1")).unwrap();
    let system_text = std::fs::read(&system).unwrap();
    let system_time = std::fs::metadata(&system).unwrap().modified().unwrap();

    // No user layout: the system one, DP-3 primary.
    {
        let Some(bus) = TestBus::start() else { return };
        let services = start(&bus, Store::with_system(user.clone(), vec![system.clone()])).await;
        assert_eq!(primary(&services).await, "DP-3");
        assert!(!user.exists(), "restoring writes nothing");
    }

    // The user's own layout wins.
    Store::at(user.clone()).save(&key, &primary_left("DVI-D-1", "DP-3")).unwrap();
    {
        let Some(bus) = TestBus::start() else { return };
        let services = start(&bus, Store::with_system(user.clone(), vec![system.clone()])).await;
        assert_eq!(primary(&services).await, "DVI-D-1");
        // Remembering goes to the user's file only.
        services.connection().object_server().interface::<_, helix_comp::display_config::DisplayConfig>("/org/gnome/Mutter/DisplayConfig").await.unwrap().get().await.remember(&key, &primary_left("DP-3", "DVI-D-1"));
        assert_eq!(Store::at(user.clone()).load(&key), Some(primary_left("DP-3", "DVI-D-1")));
    }

    assert_eq!(std::fs::read(&system).unwrap(), system_text, "the system file is never written");
    assert_eq!(std::fs::metadata(&system).unwrap().modified().unwrap(), system_time);
    let _ = std::fs::remove_dir_all(&dir);
}
