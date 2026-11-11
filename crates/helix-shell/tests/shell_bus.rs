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

/// Signals heard: ("Activated" | "Deactivated", action).
async fn accelerator_signals(conn: &zbus::Connection) -> zbus::MessageStream {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.gnome.Shell")
        .unwrap()
        .build();
    zbus::MessageStream::for_match_rule(rule, conn, None).await.unwrap()
}

async fn next_accelerator_signal(signals: &mut zbus::MessageStream) -> Option<(String, u32)> {
    use futures_util::StreamExt;
    loop {
        let message = tokio::time::timeout(Duration::from_millis(500), signals.next()).await.ok()??.ok()?;
        let member = message.header().member()?.to_string();
        if let Some(kind) = member.strip_prefix("Accelerator") {
            let (action, _): (u32, HashMap<String, zbus::zvariant::OwnedValue>) = message.body().deserialize().ok()?;
            return Some((kind.to_owned(), action));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn release_signals_and_lock_screen_mode() {
    use helix_shell::UiRequest;
    let Some(bus) = TestBus::start() else { return };
    let (service, mut ui) = ShellService::start(bus.builder()).await.unwrap();
    let bridge = service.ui_bridge();
    let client = bus.connect().await;
    let shell = proxy(&client).await;
    let mut signals = accelerator_signals(&client).await;

    let plain = shell.grab_accelerator("<Super>p", 1, 0).await.unwrap();
    let on_release = shell.grab_accelerator("<Super>r", 1, 128).await.unwrap();
    let anywhere = shell.grab_accelerator("XF86AudioMute", 1 | 4, 0).await.unwrap();
    // The lock screen grab is bound to work while locked.
    let mut locked_bind = None;
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(300), ui.recv()).await {
        if let helix_shell::UiEvent::Bind { action, when_locked, .. } = event {
            if action == anywhere {
                locked_bind = Some(when_locked);
            } else {
                assert!(!when_locked);
            }
        }
    }
    assert_eq!(locked_bind, Some(true));

    let key = |action, pressed| UiRequest::Accelerator { action, pressed, time: 1 };
    bridge.handle(key(plain, true)).await.unwrap();
    bridge.handle(key(plain, false)).await.unwrap();
    bridge.handle(key(on_release, true)).await.unwrap();
    bridge.handle(key(on_release, false)).await.unwrap();
    let mut heard = Vec::new();
    while let Some(signal) = next_accelerator_signal(&mut signals).await {
        heard.push(signal);
    }
    assert_eq!(
        heard,
        [("Activated".into(), plain), ("Activated".into(), on_release), ("Deactivated".into(), on_release)],
        "releases only with trigger-on-release"
    );

    // Locked: only the lock-screen grab fires.
    bridge.handle(UiRequest::Locked).await.unwrap();
    bridge.handle(key(plain, true)).await.unwrap();
    bridge.handle(key(anywhere, true)).await.unwrap();
    let mut heard = Vec::new();
    while let Some(signal) = next_accelerator_signal(&mut signals).await {
        heard.push(signal);
    }
    assert_eq!(heard, [("Activated".into(), anywhere)]);
}
