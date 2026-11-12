//! Every exported D-Bus interface must match its contract XML in
//! helix-desktop-spec. Members not implemented yet are listed explicitly;
//! the check fails if a listed member gets implemented, so update the list.

use helix_test_support::conformance::{assert_conformant, check, introspect, load_dbus_contract};
use helix_test_support::TestBus;

#[tokio::test(flavor = "multi_thread")]
async fn org_gnome_session_manager() {
    const IFACE: &str = "org.gnome.SessionManager";
    // Tracked in helix-desktop-spec/gaps.md.
    const KNOWN_UNIMPLEMENTED: &[&str] = &["method SetRebootToFirmwareSetup"];

    let Some(bus) = TestBus::start() else { return };
    let service = helix_session::SessionService::start(bus.builder())
        .await
        .unwrap();
    let actual = introspect(service.connection(), IFACE, "/org/gnome/SessionManager").await;
    let problems = check(
        IFACE,
        &load_dbus_contract("org.gnome.SessionManager.xml"),
        &actual,
        KNOWN_UNIMPLEMENTED,
    );
    assert_conformant(IFACE, &problems);
}

/// Introspect `path` on `conn` and check `iface` against `contract_file`.
async fn assert_matches_contract(
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
    iface: &str,
    contract_file: &str,
    known_unimplemented: &[&str],
) {
    let actual = introspect(conn, dest, path).await;
    let problems = check(iface, &load_dbus_contract(contract_file), &actual, known_unimplemented);
    assert_conformant(iface, &problems);
}

#[tokio::test(flavor = "multi_thread")]
async fn portal_backends() {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use helix_portals::{PortalService, BUS_NAME, OBJECT_PATH};
    use zbus::zvariant::{ObjectPath, Value};

    let Some(bus) = TestBus::start() else { return };
    let session = helix_session::SessionService::start(bus.builder()).await.unwrap();
    let _session = tokio::spawn(session.run(std::future::pending()));
    let portals = PortalService::start(bus.builder(), Arc::new(Mutex::new(Default::default())))
        .await
        .unwrap();
    let conn = portals.connection();

    for iface in ["Settings", "Inhibit", "Screenshot", "ScreenCast", "RemoteDesktop", "Clipboard"] {
        let name = format!("org.freedesktop.impl.portal.{iface}");
        assert_matches_contract(conn, BUS_NAME, OBJECT_PATH, &name, &format!("{name}.xml"), &[]).await;
    }

    // Request and Session objects only exist once created; make one of each.
    let client = bus.connect().await;
    let request = ObjectPath::try_from(format!("{OBJECT_PATH}/request/1_1/c1")).unwrap();
    let session = ObjectPath::try_from(format!("{OBJECT_PATH}/session/1_1/c1")).unwrap();
    client
        .call_method(
            Some(BUS_NAME),
            OBJECT_PATH,
            Some("org.freedesktop.impl.portal.Inhibit"),
            "Inhibit",
            &(&request, "org.example.App", "", 8u32, HashMap::<&str, Value<'_>>::new()),
        )
        .await
        .unwrap();
    client
        .call_method(
            Some(BUS_NAME),
            OBJECT_PATH,
            Some("org.freedesktop.impl.portal.Inhibit"),
            "CreateMonitor",
            &(&request, &session, "org.example.App", ""),
        )
        .await
        .unwrap();
    for (iface, path) in [("Request", &request), ("Session", &session)] {
        let name = format!("org.freedesktop.impl.portal.{iface}");
        assert_matches_contract(conn, BUS_NAME, path.as_str(), &name, &format!("{name}.xml"), &[]).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mutter_idle_monitor_and_display_config() {
    use helix_comp::{display_config, idle_monitor, CompositorServices};

    let Some(bus) = TestBus::start() else { return };
    let services = CompositorServices::start(bus.builder(), vec![]).await.unwrap();
    let conn = services.connection();
    assert_matches_contract(
        conn,
        idle_monitor::BUS_NAME,
        idle_monitor::CORE_PATH,
        "org.gnome.Mutter.IdleMonitor",
        "org.gnome.Mutter.IdleMonitor.xml",
        &[],
    )
    .await;
    // Write side is not implemented yet (see helix-desktop-spec/gaps.md).
    assert_matches_contract(
        conn,
        display_config::BUS_NAME,
        display_config::OBJECT_PATH,
        "org.gnome.Mutter.DisplayConfig",
        "org.gnome.Mutter.DisplayConfig.xml",
        &[
            "method ApplyConfiguration",
            "method ChangeBacklight",
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mutter_screen_cast() {
    use helix_comp::screencast::{CastRequest, BUS_NAME, OBJECT_PATH};
    use helix_comp::{CompositorServices, MonitorControl};
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedObjectPath, Value};

    let Some(bus) = TestBus::start() else { return };
    // The recording itself is not needed to check the interfaces.
    let (cast, _requests) = tokio::sync::mpsc::unbounded_channel::<CastRequest>();
    let store = std::env::temp_dir().join(format!("helix-contract-cast-{}.toml", std::process::id()));
    let control = MonitorControl { store: helix_comp::display_config::Store::at(store), cast: Some(cast), ..MonitorControl::default() };
    let monitor = helix_comp::display_config::Monitor {
        connector: "WINIT-1".into(),
        vendor: "Helix".into(),
        product: "Nested".into(),
        serial: "0".into(),
        display_name: "Helix nested window".into(),
        builtin: false,
        modes: vec![helix_comp::display_config::Mode { width: 1280, height: 800, refresh_mhz: 60_000, preferred: true, current: true }],
        x: 0,
        y: 0,
        scale: 1.0,
        transform: 0,
        primary: true,
        transforms: vec![0],
        hdr: false,
        color_mode: 0,
    };
    let services = CompositorServices::start_with(bus.builder(), vec![monitor], control).await.unwrap();
    let conn = services.connection();
    // Sessions are only for Helix's portal backend.
    let portal = bus.connect().await;
    portal.request_name("org.freedesktop.impl.portal.desktop.helix").await.unwrap();
    let no_props: HashMap<&str, Value<'_>> = HashMap::new();
    let session: OwnedObjectPath = portal
        .call_method(Some(BUS_NAME), OBJECT_PATH, Some("org.gnome.Mutter.ScreenCast"), "CreateSession", &(no_props.clone(),))
        .await
        .unwrap()
        .body()
        .deserialize()
        .unwrap();
    let stream: OwnedObjectPath = portal
        .call_method(Some(BUS_NAME), session.as_str(), Some("org.gnome.Mutter.ScreenCast.Session"), "RecordMonitor", &("WINIT-1", no_props))
        .await
        .unwrap()
        .body()
        .deserialize()
        .unwrap();
    for (path, iface) in [
        (OBJECT_PATH, "org.gnome.Mutter.ScreenCast"),
        (session.as_str(), "org.gnome.Mutter.ScreenCast.Session"),
        (stream.as_str(), "org.gnome.Mutter.ScreenCast.Stream"),
    ] {
        assert_matches_contract(conn, BUS_NAME, path, iface, "org.gnome.Mutter.ScreenCast.xml", &[]).await;
    }
    assert_matches_contract(conn, BUS_NAME, OBJECT_PATH, "org.helix.ScreenCastState", "org.helix.ScreenCastState.xml", &[]).await;

    // Remote desktop sessions.
    let remote: OwnedObjectPath = portal
        .call_method(Some("org.gnome.Mutter.RemoteDesktop"), "/org/gnome/Mutter/RemoteDesktop", Some("org.gnome.Mutter.RemoteDesktop"), "CreateSession", &())
        .await
        .unwrap()
        .body()
        .deserialize()
        .unwrap();
    for (path, iface) in [("/org/gnome/Mutter/RemoteDesktop", "org.gnome.Mutter.RemoteDesktop"), (remote.as_str(), "org.gnome.Mutter.RemoteDesktop.Session")] {
        assert_matches_contract(conn, "org.gnome.Mutter.RemoteDesktop", path, iface, "org.gnome.Mutter.RemoteDesktop.xml", &[]).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn org_gnome_shell() {
    let Some(bus) = TestBus::start() else { return };
    let (service, _ui) = helix_shell::ShellService::start(bus.builder()).await.unwrap();
    assert_matches_contract(
        service.connection(),
        helix_shell::BUS_NAME,
        helix_shell::OBJECT_PATH,
        "org.gnome.Shell",
        "org.gnome.Shell.xml",
        &[],
    )
    .await;
    assert_matches_contract(
        service.connection(),
        helix_shell::BUS_NAME,
        helix_shell::display_change::OBJECT_PATH,
        "org.helix.DisplayChangeDialog",
        "org.helix.DisplayChangeDialog.xml",
        &[],
    )
    .await;
    assert_matches_contract(
        service.connection(),
        helix_shell::BUS_NAME,
        helix_shell::night_light::OBJECT_PATH,
        "org.gnome.SettingsDaemon.Color",
        "org.gnome.SettingsDaemon.Color.xml",
        &[],
    )
    .await;
    assert_matches_contract(
        service.connection(),
        helix_shell::BUS_NAME,
        helix_shell::screencast_chooser::OBJECT_PATH,
        "org.helix.ScreenCastChooser",
        "org.helix.ScreenCastChooser.xml",
        &[],
    )
    .await;
    assert_matches_contract(
        service.connection(),
        helix_shell::BUS_NAME,
        helix_shell::screenshot::OBJECT_PATH,
        "org.helix.ScreenshotWindowPicker",
        "org.helix.ScreenshotWindowPicker.xml",
        &[],
    )
    .await;
    assert_matches_contract(
        service.connection(),
        helix_shell::BUS_NAME,
        helix_shell::screenshot::OBJECT_PATH,
        "org.gnome.Shell.Screenshot",
        "org.gnome.Shell.Screenshot.xml",
        &[],
    )
    .await;
    assert_matches_contract(
        service.connection(),
        helix_shell::recording::BUS_NAME,
        helix_shell::recording::OBJECT_PATH,
        "org.gnome.Shell.Screencast",
        "org.gnome.Shell.Screencast.xml",
        &[],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn org_gnome_screensaver() {
    let Some(bus) = TestBus::start() else { return };
    let (service, _ui) = helix_shell::ShellService::start(bus.builder()).await.unwrap();
    assert_matches_contract(
        service.connection(),
        helix_shell::screensaver::BUS_NAME,
        helix_shell::screensaver::OBJECT_PATH,
        "org.gnome.ScreenSaver",
        "org.gnome.ScreenSaver.xml",
        &[],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn notifications() {
    let Some(bus) = TestBus::start() else { return };
    let (service, _ui) = helix_shell::ShellService::start(bus.builder()).await.unwrap();
    use helix_shell::notifications::{FDO_BUS_NAME, FDO_PATH, GTK_BUS_NAME, GTK_PATH};
    assert_matches_contract(
        service.connection(),
        FDO_BUS_NAME,
        FDO_PATH,
        "org.freedesktop.Notifications",
        "org.freedesktop.Notifications.xml",
        &[],
    )
    .await;
    assert_matches_contract(service.connection(), GTK_BUS_NAME, GTK_PATH, "org.gtk.Notifications", "org.gtk.Notifications.xml", &[]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn end_session_dialog_and_session_objects() {
    let Some(bus) = TestBus::start() else { return };
    let (shell, _ui) = helix_shell::ShellService::start(bus.builder()).await.unwrap();
    assert_matches_contract(
        shell.connection(),
        helix_shell::BUS_NAME,
        helix_shell::end_dialog::OBJECT_PATH,
        "org.gnome.SessionManager.EndSessionDialog",
        "org.gnome.SessionManager.EndSessionDialog.xml",
        &[],
    )
    .await;

    // A client and an inhibitor object exported by the session manager.
    let session = helix_session::SessionService::start(bus.builder()).await.unwrap();
    let client = bus.connect().await;
    let reply = client
        .call_method(Some("org.gnome.SessionManager"), "/org/gnome/SessionManager", Some("org.gnome.SessionManager"), "RegisterClient", &("org.example.App", ""))
        .await
        .unwrap();
    let client_path: zbus::zvariant::OwnedObjectPath = reply.body().deserialize().unwrap();
    let reply = client
        .call_method(Some("org.gnome.SessionManager"), "/org/gnome/SessionManager", Some("org.gnome.SessionManager"), "Inhibit", &("org.example.App", 0u32, "busy", 1u32))
        .await
        .unwrap();
    let cookie: u32 = reply.body().deserialize().unwrap();
    let conn = session.connection();
    assert_matches_contract(conn, "org.gnome.SessionManager", client_path.as_str(), "org.gnome.SessionManager.ClientPrivate", "org.gnome.SessionManager.ClientPrivate.xml", &[]).await;
    let inhibitor = format!("/org/gnome/SessionManager/Inhibitor{cookie}");
    assert_matches_contract(conn, "org.gnome.SessionManager", &inhibitor, "org.gnome.SessionManager.Inhibitor", "org.gnome.SessionManager.Inhibitor.xml", &[]).await;
}

/// The fake logind the tests use must match the logind subset in the spec,
/// so that what Helix calls is what the spec says logind offers.
#[tokio::test(flavor = "multi_thread")]
async fn fake_logind_matches_the_login1_contract() {
    use helix_test_support::logind::{FakeLogind, BUS_NAME, MANAGER_PATH, SESSION_PATH};
    let Some(bus) = TestBus::start() else { return };
    let _fake = FakeLogind::start(&bus).await;
    let conn = bus.connect().await;
    for (path, iface) in [
        (MANAGER_PATH, "org.freedesktop.login1.Manager"),
        (SESSION_PATH, "org.freedesktop.login1.Session"),
    ] {
        assert_matches_contract(&conn, BUS_NAME, path, iface, "org.freedesktop.login1.xml", &[]).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn polkit_authentication_agent() {
    let Some(bus) = TestBus::start() else { return };
    let (ui, _rx) = tokio::sync::mpsc::unbounded_channel();
    let agent = bus
        .builder()
        .serve_at(helix_shell::polkit::AGENT_PATH, helix_shell::polkit::Agent::new(ui))
        .unwrap()
        .name("org.helix.TestAgent")
        .unwrap()
        .build()
        .await
        .unwrap();
    assert_matches_contract(
        &agent,
        "org.helix.TestAgent",
        helix_shell::polkit::AGENT_PATH,
        "org.freedesktop.PolicyKit1.AuthenticationAgent",
        "org.freedesktop.PolicyKit1.AuthenticationAgent.xml",
        &[],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn keyring_prompter() {
    let Some(bus) = TestBus::start() else { return };
    let (service, _ui) = helix_shell::ShellService::start(bus.builder()).await.unwrap();
    assert_matches_contract(
        service.connection(),
        helix_shell::keyring_prompt::BUS_NAME,
        helix_shell::keyring_prompt::OBJECT_PATH,
        "org.gnome.keyring.internal.Prompter",
        "org.gnome.keyring.internal.Prompter.xml",
        &[],
    )
    .await;
}
