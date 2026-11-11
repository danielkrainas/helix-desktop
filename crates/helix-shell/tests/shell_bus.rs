//! org.gnome.Shell over a private bus.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use helix_shell::{ShellService, UiEvent, SHELL_VERSION};
use helix_test_support::TestBus;
use zbus::proxy::CacheProperties;
use zbus::zvariant::Value;

#[zbus::proxy(
    interface = "org.gnome.Shell",
    default_service = "org.gnome.Shell",
    default_path = "/org/gnome/Shell"
)]
trait Shell {
    fn eval(&self, script: &str) -> zbus::Result<(bool, String)>;
    fn focus_search(&self) -> zbus::Result<()>;
    #[zbus(name = "ShowOSD")]
    fn show_osd(&self, params: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    fn grab_accelerator(&self, accelerator: &str, mode_flags: u32, grab_flags: u32) -> zbus::Result<u32>;
    fn grab_accelerators(&self, accelerators: &[(&str, u32, u32)]) -> zbus::Result<Vec<u32>>;
    fn ungrab_accelerator(&self, action: u32) -> zbus::Result<bool>;
    fn ungrab_accelerators(&self, actions: &[u32]) -> zbus::Result<bool>;
    #[zbus(property)]
    fn mode(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn shell_version(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn overview_active(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_overview_active(&self, active: bool) -> zbus::Result<()>;
}

async fn proxy(conn: &zbus::Connection) -> ShellProxy<'static> {
    ShellProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn properties_eval_overview_and_osd() {
    let Some(bus) = TestBus::start() else { return };
    let (_service, mut ui) = ShellService::start(bus.builder()).await.unwrap();
    let conn = bus.connect().await;
    let shell = proxy(&conn).await;

    assert_eq!(shell.mode().await.unwrap(), "user");
    assert_eq!(shell.shell_version().await.unwrap(), SHELL_VERSION);
    assert_eq!(shell.eval("1 + 1").await.unwrap(), (false, String::new()));

    assert!(!shell.overview_active().await.unwrap());
    shell.focus_search().await.unwrap();
    assert!(shell.overview_active().await.unwrap());
    assert_eq!(ui.recv().await, Some(UiEvent::Overview(true)));
    shell.set_overview_active(false).await.unwrap();
    assert!(!shell.overview_active().await.unwrap());
    assert_eq!(ui.recv().await, Some(UiEvent::Overview(false)));

    shell
        .show_osd(HashMap::from([
            ("icon", Value::from("audio-volume-high-symbolic")),
            ("level", Value::from(0.5f64)),
        ]))
        .await
        .unwrap();
    match ui.recv().await {
        Some(UiEvent::Osd(osd)) => {
            assert_eq!(osd.icon.as_deref(), Some("audio-volume-high-symbolic"));
            assert_eq!(osd.level, Some(0.5));
            assert_eq!(osd.max_level, 1.0);
            assert_eq!(osd.label, None);
        }
        other => panic!("expected OSD, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn accelerator_grabs() {
    let Some(bus) = TestBus::start() else { return };
    let (service, _ui) = ShellService::start(bus.builder()).await.unwrap();
    let observer = bus.connect().await;
    let shell = proxy(&observer).await;

    let a = shell.grab_accelerator("<Super>a", 1, 0).await.unwrap();
    assert_ne!(a, 0);
    assert_eq!(shell.grab_accelerator("<super>A", 1, 0).await.unwrap(), 0, "taken");
    assert_eq!(shell.grab_accelerator("<Nope>x", 1, 0).await.unwrap(), 0, "invalid");
    let many = shell
        .grab_accelerators(&[("<Super>b", 1, 0), ("bad key", 1, 0), ("XF86AudioMute", 0xFFFF_FFFF, 0)])
        .await
        .unwrap();
    assert_eq!(many.len(), 3);
    assert!(many[0] != 0 && many[1] == 0 && many[2] != 0);
    assert!(shell.ungrab_accelerator(a).await.unwrap());
    assert!(!shell.ungrab_accelerator(a).await.unwrap(), "already released");
    assert_eq!(service.grabbed().len(), 2);
    // A miss in the list still releases the others, and reports false.
    let extra = shell.grab_accelerator("<Super>c", 1, 0).await.unwrap();
    assert!(!shell.ungrab_accelerators(&[a, extra]).await.unwrap());
    assert_eq!(service.grabbed().len(), 2);

    // Grabs of a client that leaves the bus are released.
    {
        let other = bus.connect().await;
        let other_shell = proxy(&other).await;
        assert_ne!(other_shell.grab_accelerator("<Super>z", 1, 0).await.unwrap(), 0);
        assert_eq!(service.grabbed().len(), 3);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while service.grabbed().len() != 2 {
        assert!(Instant::now() < deadline, "grab not released");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
